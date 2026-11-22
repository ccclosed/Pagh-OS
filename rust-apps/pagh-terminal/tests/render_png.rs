//! Renderer, PNG and committed-screenshot tests.

use pagh_terminal::cell::Rgb;
use pagh_terminal::demo;
use pagh_terminal::png;
use pagh_terminal::render::{
    render_screen, render_screen_into, render_screen_into_cleared, render_window, Canvas,
    RenderOptions, Theme, BORDER, SELECTION_BG_MIX, TITLE_H, VIEWPORT_PADDING,
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

    // Selected cell background: a wash over the cell background, not an
    // opaque fill (see render::SELECTION_BG_MIX).
    assert_eq!(
        &canvas.pixel(VIEWPORT_PADDING + 8 + 2, VIEWPORT_PADDING + 2)[..3],
        &theme.bg.lerp(theme.selection, SELECTION_BG_MIX).to_array()
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

/// Репро бага на настоящем выводе `fastfetch`, снятом с гостя (serial-лог
/// сессии qbug3): цветной `#`-логотип слева, ANSI-текст справа и полоса
/// цветовых фонов внизу.
///
/// Оракул: кадр с выделением обязан быть пиксель-в-пиксель равен кадру без
/// выделения, отрисованному с `theme.bg == подсветка`, во всех клетках с
/// дефолтным фоном. Это ровно утверждение «выделение меняет только фон»:
/// foreground, сглаживание и ANSI-цвета совпадают по построению, а не по
/// переписанной в тесте формуле.
///
/// Прежний `resolve_style` подменял foreground на один тёмный `selection_fg`,
/// поэтому логотип и цветные пары ключ/значение становились серыми, а
/// ANSI-фоны — сплошной синей полосой.
#[test]
fn selection_over_real_fastfetch_output_keeps_logo_text_and_colours() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read(dir.join("tests/fixtures/fastfetch-guest.script"))
        .expect("fastfetch guest fixture");
    let theme = theme();
    let opts = RenderOptions::default();
    let mut plain = Terminal::new(100, 30);
    plain.feed(&raw);
    let before = render_screen(&plain.screen, &theme, &opts);

    // Весь блок fastfetch: 12 строк логотипа/информации + две строки
    // цветовых фонов (ANSI 40..47 и 100..107).
    let mut selected = Terminal::new(100, 30);
    selected.feed(&raw);
    selected
        .screen
        .begin_selection_abs(0, 0, SelectionMode::Char);
    selected.screen.extend_selection_abs(13, 99);
    let after = render_screen(&selected.screen, &theme, &opts);

    let highlight = theme.bg.lerp(theme.selection, SELECTION_BG_MIX);
    let mut washed = theme.clone();
    washed.bg = highlight;
    let oracle = render_screen(&plain.screen, &washed, &opts);
    assert_ne!(
        highlight.to_array(),
        theme.selection.to_array(),
        "selection wash must not be the opaque accent"
    );

    let rgb = |px: [u8; 4]| [px[0], px[1], px[2]];
    let mut background = 0usize;
    let mut glyph = 0usize;
    let mut swatches = 0usize;
    for row in 0..14 {
        let line = plain.screen.view_line(row);
        for col in 0..100 {
            let cell = line.get(col).copied().unwrap_or_default();
            for y in 0..18 {
                for x in 0..8 {
                    let px = (
                        VIEWPORT_PADDING + col * 8 + x,
                        VIEWPORT_PADDING + row * 18 + y,
                    );
                    let b = rgb(before.pixel(px.0, px.1));
                    let a = after.pixel(px.0, px.1);
                    if !cell.bg.is_default() {
                        // ANSI-фон (полоса цветов внизу): подсветка — смесь, а
                        // не сплошная заливка, поэтому арт остаётся различимым.
                        let bg = cell.bg.resolve(&theme.palette, theme.bg);
                        assert_eq!(
                            rgb(a),
                            bg.lerp(theme.selection, SELECTION_BG_MIX).to_array(),
                            "ANSI background art was erased at ({row},{col})"
                        );
                        assert_ne!(rgb(a), theme.selection.to_array());
                        swatches += 1;
                        continue;
                    }
                    assert_eq!(
                        a,
                        oracle.pixel(px.0, px.1),
                        "selected cell ({row},{col}) differs from `theme.bg = highlight`"
                    );
                    if b == theme.bg.to_array() {
                        assert_eq!(rgb(a), highlight.to_array());
                        background += 1;
                    } else {
                        glyph += 1;
                    }
                }
            }
        }
    }
    assert!(
        background > 10_000,
        "selection painted too little: {background}"
    );
    assert!(
        swatches > 100,
        "no ANSI colour swatches covered: {swatches}"
    );

    // Регрессия «foreground посерел»: ядра глифов (полное покрытие) — это
    // пиксели, которые в оракуле остались прежними; старый код перекрашивал
    // их все, поэтому счётчик обнулялся бы. Края сглаживания законно
    // смешиваются с новым фоном, поэтому их сюда не относим.
    let mut preserved = 0usize;
    for row in 0..14 {
        let line = plain.screen.view_line(row);
        for col in 0..100 {
            let cell = line.get(col).copied().unwrap_or_default();
            if !cell.bg.is_default() {
                continue;
            }
            for y in 0..18 {
                for x in 0..8 {
                    let px = (
                        VIEWPORT_PADDING + col * 8 + x,
                        VIEWPORT_PADDING + row * 18 + y,
                    );
                    let b = before.pixel(px.0, px.1);
                    let a = after.pixel(px.0, px.1);
                    if b[..3] != theme.bg.to_array()[..] && b == a {
                        preserved += 1;
                    }
                }
            }
        }
    }
    assert!(
        preserved > 1_000,
        "foreground colours were not preserved: {preserved}"
    );
    // И сам кадр не должен быть «залит» подсветкой: глифовые пиксели заведомо
    // есть внутри выделения.
    assert!(
        glyph > 1_000,
        "no glyph pixels inside the selection: {glyph}"
    );
}

/// Whole-area selection (`SelectionMode::Area` через `select_all_view`):
/// выделяется весь видимый вывод целиком, а не прямоугольник, у которого
/// первая/последняя строка обрезаны по колонке нажатия/отпускания.
///
/// Оракул тот же, что у теста выше — кадр обязан совпасть с рендером, где
/// `theme.bg` уже равен подсветке, — но теперь он применяется к *каждой*
/// клетке сетки: ни одна колонка логотипа и ни один свотч палитры не остаётся
/// вне выделения и не сереет.
#[test]
fn whole_area_selection_covers_whole_output_and_keeps_colours() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read(dir.join("tests/fixtures/fastfetch-guest.script"))
        .expect("fastfetch guest fixture");
    let theme = theme();
    let opts = RenderOptions {
        show_cursor: false,
        ..RenderOptions::default()
    };

    let mut plain = Terminal::new(100, 30);
    plain.feed(&raw);
    let before = render_screen(&plain.screen, &theme, &opts);

    let mut selected = Terminal::new(100, 30);
    selected.feed(&raw);
    selected.screen.select_all_view();
    assert_eq!(
        selected.screen.selection().unwrap().mode,
        SelectionMode::Area,
        "select_all_view must use whole-area mode"
    );
    let after = render_screen(&selected.screen, &theme, &opts);

    let highlight = theme.bg.lerp(theme.selection, SELECTION_BG_MIX);
    let mut washed_theme = theme.clone();
    washed_theme.bg = highlight;
    let oracle = render_screen(&plain.screen, &washed_theme, &opts);

    let rgb = |px: [u8; 4]| [px[0], px[1], px[2]];
    let mut washed = 0usize;
    let mut glyph_core = 0usize;
    let mut glyph_colours = std::collections::BTreeSet::new();
    let mut swatches = 0usize;
    for row in 0..30 {
        let line = plain.screen.view_line(row);
        for col in 0..100 {
            let cell = line.get(col).copied().unwrap_or_default();
            for y in 0..18 {
                for x in 0..8 {
                    let px = (
                        VIEWPORT_PADDING + col * 8 + x,
                        VIEWPORT_PADDING + row * 18 + y,
                    );
                    let b = before.pixel(px.0, px.1);
                    let a = after.pixel(px.0, px.1);
                    if !cell.bg.is_default() {
                        let bg = cell.bg.resolve(&theme.palette, theme.bg);
                        assert_eq!(
                            rgb(a),
                            bg.lerp(theme.selection, SELECTION_BG_MIX).to_array(),
                            "ANSI background art erased at ({row},{col})"
                        );
                        swatches += 1;
                        continue;
                    }
                    assert_eq!(
                        a,
                        oracle.pixel(px.0, px.1),
                        "whole-area cell ({row},{col}) differs from the wash oracle"
                    );
                    if b[..3] == theme.bg.to_array()[..] {
                        assert_eq!(
                            rgb(a),
                            highlight.to_array(),
                            "cell ({row},{col}) was not selected"
                        );
                        washed += 1;
                    } else if b == a {
                        glyph_core += 1;
                        glyph_colours.insert(rgb(b));
                    }
                }
            }
        }
    }
    // Вся сетка 100x30, от верхней строки до нижней и от первой колонки до
    // последней: подсветка есть в каждой клетке, включая пустые.
    assert!(washed > 26_000, "whole area not washed: {washed}");
    assert!(swatches > 100, "ANSI palette strip missing: {swatches}");
    assert!(glyph_core > 2_000, "logo/text pixels lost: {glyph_core}");
    assert!(
        glyph_colours.len() >= 3,
        "foreground collapsed into {} colour(s)",
        glyph_colours.len()
    );
}
