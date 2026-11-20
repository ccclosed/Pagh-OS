//! Golden tests for the VT100/ANSI parser.
//!
//! Each test feeds a byte string that a real application (bash, vim, neovim,
//! htop, apt) is known to emit and asserts the resulting screen state.  These
//! are deliberately behavior-level assertions (text, cursor, colors, modes),
//! not parser-internal snapshots, so they survive a refactor.

use pagh_terminal::cell::{Attrs, Color};
use pagh_terminal::screen::SelectionMode;
use pagh_terminal::Terminal;

fn term(cols: usize, rows: usize) -> Terminal {
    Terminal::new(cols, rows)
}

fn feed(t: &mut Terminal, bytes: &str) {
    t.feed_str(bytes);
}

fn lines(t: &Terminal) -> Vec<String> {
    (0..t.screen.rows())
        .map(|y| t.screen.line_text(y))
        .collect()
}

#[test]
fn plain_text_and_crlf() {
    let mut t = term(20, 3);
    feed(&mut t, "hello\r\nworld");
    assert_eq!(t.screen.text(), "hello\nworld");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (5, 1));
}

#[test]
fn deferred_autowrap_and_autowrap_off() {
    let mut t = term(5, 2);
    feed(&mut t, "abcde");
    assert_eq!(t.screen.cursor.x, 4);
    assert!(t.screen.cursor.wrap_pending);
    feed(&mut t, "f");
    assert_eq!(lines(&t), vec!["abcde", "f"]);
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (1, 1));

    // DECAWM off: the last column is overwritten rather than wrapping.
    feed(&mut t, "\x1b[2;1H12345\x1b[?7lX");
    assert_eq!(t.screen.line_text(1), "1234X");
}

#[test]
fn cursor_positioning_and_limits() {
    let mut t = term(10, 5);
    feed(&mut t, "\x1b[2;3H");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (2, 1));
    feed(&mut t, "\x1b[2A\x1b[3D");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (0, 0));
    feed(&mut t, "\x1b[5G\x1b[4d");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (4, 3));
    feed(&mut t, "\x1b[100C\x1b[100B");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (9, 4));
    feed(&mut t, "\x1b[2E");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (0, 4));
    feed(&mut t, "\x1b[2F");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (0, 2));
    feed(&mut t, "\x1b[5a\x1b[2e");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (5, 4));
}

#[test]
fn erase_display_line_and_chars() {
    let mut t = term(6, 3);
    for row in 0..3 {
        feed(&mut t, &format!("\x1b[{};1Habcdef", row + 1));
    }
    feed(&mut t, "\x1b[2;3H\x1b[K");
    assert_eq!(t.screen.line_text(1), "ab");
    feed(&mut t, "\x1b[2;1Habcdef\x1b[2;3H\x1b[1K");
    assert_eq!(t.screen.line_text(1), "   def");
    feed(&mut t, "\x1b[2;1Habcdef\x1b[2;3H\x1b[2X");
    assert_eq!(t.screen.line_text(1), "ab  ef");
    feed(&mut t, "\x1b[2J");
    assert_eq!(t.screen.text(), "");
    // ED 3 clears only the scrollback.
    assert_eq!(t.screen.scrollback_len(), 0);
}

#[test]
fn insert_and_delete_characters() {
    let mut t = term(8, 2);
    feed(&mut t, "abcdef");
    feed(&mut t, "\x1b[1;3H\x1b[2@");
    assert_eq!(t.screen.line_text(0), "ab  cdef");
    feed(&mut t, "\x1b[1;3H\x1b[2P");
    assert_eq!(t.screen.line_text(0), "abcdef");
    feed(&mut t, "\x1b[1;1H\x1b[3P");
    assert_eq!(t.screen.line_text(0), "def");
}

#[test]
fn insert_and_delete_lines() {
    let mut t = term(6, 4);
    for (i, line) in ["one", "two", "three"].iter().enumerate() {
        feed(&mut t, &format!("\x1b[{};1H{line}", i + 1));
    }
    feed(&mut t, "\x1b[2;1H\x1b[L");
    assert_eq!(lines(&t), vec!["one", "", "two", "three"]);
    feed(&mut t, "\x1b[2;1H\x1b[M");
    assert_eq!(lines(&t), vec!["one", "two", "three", ""]);
}

#[test]
fn scroll_region_lf_and_reverse_index() {
    let mut t = term(6, 5);
    feed(&mut t, "top\x1b[2;4r\x1b[2;1HA\x1b[3;1HB\x1b[4;1HC\r\nD");
    assert_eq!(t.screen.line_text(0), "top");
    assert_eq!(t.screen.line_text(1), "B");
    assert_eq!(t.screen.line_text(2), "C");
    assert_eq!(t.screen.line_text(3), "D");
    assert_eq!(t.screen.line_text(4), "");
    assert_eq!(t.screen.scrollback_len(), 0);

    // Reverse index at the top margin scrolls the region down.
    feed(&mut t, "\x1b[2;1H\x1bM");
    assert_eq!(t.screen.line_text(0), "top");
    assert_eq!(t.screen.line_text(1), "");
    assert_eq!(t.screen.line_text(2), "B");
}

#[test]
fn full_screen_scroll_pushes_scrollback() {
    let mut t = term(4, 2);
    feed(&mut t, "a\r\nb\r\nc");
    assert_eq!(t.screen.scrollback_len(), 1);
    assert_eq!(lines(&t), vec!["b", "c"]);
    t.screen.set_view_offset(1);
    assert_eq!(t.screen.view_text(), "a\nb");
    assert_eq!(t.screen.view_offset(), 1);
    t.screen.set_view_offset(0);
    assert_eq!(t.screen.view_text(), "b\nc");
}

#[test]
fn sgr_colors_attributes_and_reset() {
    let mut t = term(20, 3);
    feed(
        &mut t,
        "\x1b[1;31;44mA\x1b[0mB\x1b[38;5;196mC\x1b[48;5;21mD\x1b[38;2;12;34;56mE\
         \x1b[48;2;1;2;3mF\x1b[38:2::1:2:3mG\x1b[39;49mH",
    );
    assert_eq!(t.screen.cell(0, 0).fg, Color::Indexed(1));
    assert_eq!(t.screen.cell(0, 0).bg, Color::Indexed(4));
    assert!(t.screen.cell(0, 0).attrs.contains(Attrs::BOLD));
    assert_eq!(t.screen.cell(1, 0).fg, Color::Default);
    assert_eq!(t.screen.cell(2, 0).fg, Color::Indexed(196));
    assert_eq!(t.screen.cell(3, 0).bg, Color::Indexed(21));
    assert_eq!(t.screen.cell(4, 0).fg, Color::Rgb(12, 34, 56));
    assert_eq!(t.screen.cell(5, 0).bg, Color::Rgb(1, 2, 3));
    assert_eq!(t.screen.cell(6, 0).fg, Color::Rgb(1, 2, 3));
    assert_eq!(t.screen.cell(7, 0).fg, Color::Default);
    assert_eq!(t.screen.cell(7, 0).bg, Color::Default);

    // 38;2 with fewer than three components must not eat the next SGR.
    feed(&mut t, "\x1b[38;2;7mX\x1b[0mY");
    assert_eq!(t.screen.cell(8, 0).fg, Color::Default);
}

#[test]
fn sgr_attributes_off_and_colon_underline() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b[1;2;3;4;5;7;8;9mX");
    let attrs = t.screen.cell(0, 0).attrs;
    for flag in [
        Attrs::BOLD,
        Attrs::DIM,
        Attrs::ITALIC,
        Attrs::UNDERLINE,
        Attrs::BLINK,
        Attrs::REVERSE,
        Attrs::HIDDEN,
        Attrs::STRIKETHROUGH,
    ] {
        assert!(attrs.contains(flag), "missing {flag:?}");
    }
    feed(&mut t, "\x1b[22;23;24;25;27;28;29mY");
    assert_eq!(t.screen.cell(1, 0).attrs, Attrs::empty());

    // Colon subparameters: 4:3 sets underline and only consumes the style.
    feed(&mut t, "\x1b[4:3;31mZ");
    assert!(t.screen.cell(2, 0).attrs.contains(Attrs::UNDERLINE));
    assert_eq!(t.screen.cell(2, 0).fg, Color::Indexed(1));
}

#[test]
fn bce_erased_cells_use_current_background() {
    let mut t = term(5, 2);
    feed(&mut t, "\x1b[44m\x1b[2J");
    assert_eq!(t.screen.cell(0, 0).bg, Color::Indexed(4));
    feed(&mut t, "\x1b[0m\x1b[2J");
    assert_eq!(t.screen.cell(0, 0).bg, Color::Default);
}

#[test]
fn cursor_save_restore_and_origin_mode() {
    let mut t = term(10, 6);
    feed(&mut t, "\x1b[3;4H\x1b7\x1b[6;1H\x1b8");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (3, 2));

    feed(&mut t, "\x1b[2;4r\x1b[?6h\x1b[1;1H");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (0, 1));
    feed(&mut t, "\x1b[2;1H");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (0, 2));
    feed(&mut t, "\x1b[?6l\x1b[1;1H");
    assert_eq!((t.screen.cursor.x, t.screen.cursor.y), (0, 0));
}

#[test]
fn tab_stops_set_clear_and_move_both_ways() {
    let mut t = term(30, 2);
    feed(&mut t, "\x1b[1;5H\x1bH"); // HTS at 0-based column 4
    feed(&mut t, "\x1b[1;1H\t");
    assert_eq!(t.screen.cursor.x, 4);
    feed(&mut t, "\x1b[1;5H\x1b[g\x1b[1;1H\t");
    assert_eq!(t.screen.cursor.x, 8);
    feed(&mut t, "\x1b[1;20H\x1b[Z");
    assert_eq!(t.screen.cursor.x, 16);
    feed(&mut t, "\x1b[3g");
    assert_eq!(t.screen.cursor.x, 16);
}

#[test]
fn utf8_cyrillic_cjk_and_streaming_boundaries() {
    let mut t = term(10, 3);
    t.feed(&[0xD0]); // first byte of П
    t.feed(&[0x9F]);
    t.feed(&[0xD1, 0x80]); // р
    assert_eq!(t.screen.text(), "Пр");
    assert_eq!(t.screen.cursor.x, 2);

    feed(&mut t, "界");
    assert_eq!(t.screen.cursor.x, 4);
    assert!(t.screen.cell(2, 0).is_wide());
    assert!(t.screen.cell(3, 0).is_wide_continuation());

    // 4-byte emoji is two cells; the emoji has no bundled glyph but still
    // advances the cursor and renders as a wide placeholder.
    feed(&mut t, "\u{1F600}");
    assert_eq!(t.screen.cursor.x, 6);
    feed(&mut t, "\u{0301}");
    assert_eq!(t.screen.cursor.x, 6); // combining mark is zero width
}

#[test]
fn invalid_utf8_is_replaced_without_eating_following_ascii() {
    let mut t = term(10, 2);
    t.feed(&[0xE2, 0x28, b'A', 0xFF, b'B']);
    assert_eq!(t.screen.text(), "\u{FFFD}(A\u{FFFD}B");
}

#[test]
fn wide_character_wraps_at_the_right_edge() {
    let mut t = term(3, 2);
    feed(&mut t, "ab界");
    assert_eq!(t.screen.line_text(0), "ab");
    assert_eq!(t.screen.line_text(1), "界");
    assert_eq!(t.screen.cursor.y, 1);
}

#[test]
fn alternate_screen_restores_primary_grid_and_cursor() {
    let mut t = term(12, 3);
    feed(&mut t, "primary");
    feed(&mut t, "\x1b[?1049h\x1b[2J\x1b[Halternate");
    assert_eq!(t.screen.text(), "alternate");
    assert!(t.screen.alt_active());
    feed(&mut t, "\x1b[?1049l");
    assert_eq!(t.screen.text(), "primary");
    assert_eq!(t.screen.cursor.x, 7);
    assert!(!t.screen.alt_active());
}

#[test]
fn osc_title_and_ignored_osc_commands() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b]0;first\x07");
    assert_eq!(t.screen.title, "first");
    feed(&mut t, "\x1b]2;second\x1b\\");
    assert_eq!(t.screen.title, "second");
    feed(&mut t, "\x1b]8;;https://example.invalid/\x1b\\");
    feed(&mut t, "\x1b]52;c;ZZZ\x07");
    // Hyperlinks and clipboard writes are parsed but not surfaced/stored.
    assert_eq!(t.screen.title, "second");
    assert_eq!(t.screen.text(), "");
}

#[test]
fn dec_special_graphics_and_charset_switch() {
    let mut t = term(20, 3);
    feed(&mut t, "\x1b(0lqk\x1b(Bx");
    assert_eq!(t.screen.line_text(0), "┌─┐x");
    feed(&mut t, "\x1b[2;1H\x1b)0\x0elqk\x0f\x1b)B lqk");
    assert_eq!(t.screen.line_text(1), "┌─┐ lqk");
}

#[test]
fn device_queries_and_mode_reports() {
    let mut t = term(20, 5);
    feed(&mut t, "\x1b[2;3H");
    feed(&mut t, "\x1b[6n\x1b[c\x1b[>c\x1b[5n\x1b[18t");
    feed(&mut t, "\x1b[?2004h");
    feed(&mut t, "\x1b[?2004$p");
    feed(&mut t, "\x1b[4$p");
    feed(&mut t, "\x1b[?25$p");
    let responses = String::from_utf8_lossy(&t.take_responses()).into_owned();
    assert!(responses.contains("\x1b[2;3R"), "{responses:?}");
    assert!(responses.contains("\x1b[?62;22c"), "{responses:?}");
    assert!(responses.contains("\x1b[>69;1;0c"), "{responses:?}");
    assert!(responses.contains("\x1b[0n"), "{responses:?}");
    assert!(responses.contains("\x1b[8;5;20t"), "{responses:?}");
    assert!(responses.contains("\x1b[?2004;1$y"), "{responses:?}");
    assert!(responses.contains("\x1b[4;2$y"), "{responses:?}");
    assert!(responses.contains("\x1b[?25;1$y"), "{responses:?}");
}

#[test]
fn bracket_paste_mouse_focus_and_cursor_visibility_modes() {
    let mut t = term(10, 2);
    feed(
        &mut t,
        "\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?25l",
    );
    assert!(t.screen.bracketed_paste);
    assert!(t.screen.mouse_sgr);
    assert!(t.screen.focus_events);
    assert!(!t.screen.cursor.visible);
    assert_eq!(t.screen.mouse_mode, pagh_terminal::screen::MouseMode::Click);
    feed(&mut t, "\x1b[?1000l\x1b[?25h");
    assert_eq!(t.screen.mouse_mode, pagh_terminal::screen::MouseMode::Off);
    assert!(t.screen.cursor.visible);
}

#[test]
fn decaln_and_repeat() {
    let mut t = term(6, 3);
    feed(&mut t, "\x1b#8");
    assert!(t.screen.text().lines().all(|line| line == "EEEEEE"));
    feed(&mut t, "\x1b[2J\x1b[1;1H\x1b[31mab\x1b[3b");
    assert_eq!(t.screen.line_text(0), "abbbb");
}

#[test]
fn insert_mode_and_newline_mode() {
    let mut t = term(10, 3);
    feed(&mut t, "abc\x1b[4h\x1b[1;2HX");
    assert_eq!(t.screen.line_text(0), "aXbc");

    let mut t = term(10, 3);
    feed(&mut t, "\x1b[20hAB\nCD");
    // LNM: LF also returns to column 0.
    assert_eq!(t.screen.line_text(0), "AB");
    assert_eq!(t.screen.line_text(1), "CD");
}

#[test]
fn resize_preserves_content_and_scrollback() {
    let mut t = term(6, 3);
    feed(&mut t, "one\r\ntwo\r\nthree");
    assert_eq!(t.screen.scrollback_len(), 0);
    t.resize(8, 2);
    assert_eq!(t.screen.scrollback_len(), 1);
    assert_eq!(lines(&t)[0], "two");
    assert_eq!(lines(&t)[1], "three");
    t.resize(8, 4);
    assert_eq!(t.screen.scrollback_len(), 0);
    assert_eq!(t.screen.line_text(0), "one");
    assert_eq!(t.screen.line_text(1), "two");
    t.resize(3, 4);
    assert_eq!(t.screen.line_text(0), "one");
    assert!(t.screen.cols() == 3);
}

#[test]
fn selection_includes_multiline_and_wide_text() {
    let mut t = term(10, 3);
    feed(&mut t, "hello\r\nworld\r\n界語");
    t.screen.begin_selection_abs(0, 1, SelectionMode::Char);
    t.screen.extend_selection_abs(1, 2);
    assert_eq!(t.screen.selection_text().as_deref(), Some("ello\nwor"));

    // Reverse anchor/focus still yields forward text.
    t.screen.begin_selection_abs(2, 3, SelectionMode::Char);
    t.screen.extend_selection_abs(2, 0);
    assert_eq!(t.screen.selection_text().as_deref(), Some("界語"));
    t.screen.select_line_view(1);
    assert_eq!(t.screen.selection_text().as_deref(), Some("world"));
}

#[test]
fn malformed_escape_sequences_are_swallowed() {
    let mut t = term(20, 3);
    feed(
        &mut t,
        "a\x1b[31?mb\x1b[999999999999999999999C\x1b]0;unterminated",
    );
    // No final byte for the OSC, so the title stays empty; the junk must not
    // appear on the grid.
    assert_eq!(t.screen.line_text(0), "ab");
    assert!(t.screen.title.is_empty());
}

#[test]
fn dec_private_save_restore_modes() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b[?1h\x1b[?7l\x1b[?25l\x1b[?s");
    feed(&mut t, "\x1b[?1l\x1b[?7h\x1b[?25h");
    feed(&mut t, "\x1b[?r");
    assert!(t.screen.app_cursor);
    assert!(!t.screen.auto_wrap);
    assert!(!t.screen.cursor.visible);
}

#[test]
fn resize_while_alternate_screen_preserves_primary() {
    let mut t = term(10, 3);
    feed(&mut t, "primary");
    feed(&mut t, "\x1b[?1049hALT");
    t.resize(12, 4);
    assert_eq!(t.screen.text(), "ALT");
    feed(&mut t, "\x1b[?1049l");
    assert_eq!(t.screen.line_text(0), "primary");
    assert_eq!((t.screen.cols(), t.screen.rows()), (12, 4));
}
