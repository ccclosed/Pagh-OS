//! Regression: the console text grid must be derived from the framebuffer
//! resolution, and a corner-to-corner drag must be mappable over all of it.
//!
//! The reported bug: on a 16:9 screen the white mouse selection highlight on the
//! kernel console covered only the top half of the height. Cause: the console grid
//! was a fixed `100×37` cells (`CHARS_PER_LINE`/`MAX_LINES`) regardless of the
//! display and `text_rows()` clamped to it, so on 2560×1440 the selectable cells
//! ended at `37·18 = 666 px` — 46.2% of the height. The live probe reproduced it
//! (`x 0..799`, `y 0..665` at every resolution).
//!
//! These properties pin the fix: [`console_grid`] uses the real screen, so the
//! last grid row sits in the last band above the status bar on every shape named
//! by the acceptance, and [`cell_at`] keeps the pointer's cell inside that grid.

use crate::console_layout::{
    cell_at, console_grid, CHAR_HEIGHT, CHAR_WIDTH, MAX_COLS, MAX_ROWS, STATUS_BAR_HEIGHT,
};

/// Screen shapes the acceptance names, plus the 16:9 shape where the bug was
/// worst (46% coverage) and a 4K panel.
const ACCEPTANCE: [(usize, usize); 3] = [(1280, 720), (1920, 1080), (1280, 800)];
const EXTRA: [(usize, usize); 2] = [(2560, 1440), (3840, 2160)];

#[test]
fn acceptance_resolutions_use_the_whole_screen() {
    for (width, height) in ACCEPTANCE.iter().copied().chain(EXTRA) {
        let (cols, rows) = console_grid(width, height);
        assert_eq!(cols, width / CHAR_WIDTH, "{width}x{height}: columns");
        assert_eq!(
            rows,
            (height - STATUS_BAR_HEIGHT) / CHAR_HEIGHT,
            "{width}x{height}: rows"
        );
        // No cell may hang past the screen, and the grid must leave less than one
        // cell of unused height (the status bar takes its own row).
        assert!(
            cols * CHAR_WIDTH <= width,
            "{width}x{height}: grid wider than the screen"
        );
        assert!(
            rows * CHAR_HEIGHT + STATUS_BAR_HEIGHT <= height,
            "{width}x{height}: grid overlaps/passes the status bar"
        );
        assert!(
            height - (rows * CHAR_HEIGHT + STATUS_BAR_HEIGHT) < CHAR_HEIGHT,
            "{width}x{height}: more than one unused cell of height"
        );
        // And the grid is not the old fixed one: the selection can reach well
        // past 37 rows on any modern screen.
        if height >= 1080 {
            assert!(
                rows > 37,
                "{width}x{height}: {rows} rows is the fixed-grid regression"
            );
        }
    }
}

#[test]
fn the_reported_half_screen_shape_reaches_the_bottom_band() {
    // 2560×1440: the fixed grid's last row started at 648 px (46.2% of the
    // height). The derived grid's last row starts at 1404 px and its band ends
    // exactly where the status bar begins.
    let (cols, rows) = console_grid(2560, 1440);
    assert_eq!((cols, rows), (320, 79));
    let last_row_top = (rows - 1) * CHAR_HEIGHT;
    assert_eq!(last_row_top, 1404);
    assert_eq!(last_row_top + CHAR_HEIGHT + STATUS_BAR_HEIGHT, 1440);
    // The fixed grid's whole text area was smaller than half of this screen.
    assert!(37 * CHAR_HEIGHT * 2 < 1440);
}

#[test]
fn tiny_and_empty_screens_still_yield_a_usable_grid() {
    for (width, height) in [(0, 0), (1, 1), (CHAR_WIDTH - 1, CHAR_HEIGHT - 1), (7, 17)] {
        let (cols, rows) = console_grid(width, height);
        assert!(cols >= 1 && rows >= 1, "{width}x{height}: empty grid");
    }
    // Serial-only boot and pre-init writers ask for 0×0; the model must not be
    // sized from garbage and must stay within the ABI caps.
    assert_eq!(console_grid(0, 0), (1, 1));
    assert_eq!(console_grid(usize::MAX, usize::MAX), (MAX_COLS, MAX_ROWS));
}

#[test]
fn grid_grows_monotonically_with_the_screen() {
    let mut last = (0usize, 0usize);
    for width in (0..5000).step_by(97) {
        let (cols, _) = console_grid(width, 1080);
        assert!(cols >= last.0, "columns shrank at width {width}");
        last.0 = cols;
    }
    let mut last_rows = 0;
    for height in (0..3000).step_by(59) {
        let (_, rows) = console_grid(1920, height);
        assert!(rows >= last_rows, "rows shrank at height {height}");
        last_rows = rows;
    }
}

#[test]
fn corner_to_corner_pointer_maps_inside_the_grid() {
    for (width, height) in ACCEPTANCE.iter().copied().chain(EXTRA) {
        let (cols, rows) = console_grid(width, height);
        // Top-left corner is the first cell.
        assert_eq!(cell_at(0, 0, cols, rows), (0, 0), "{width}x{height}");
        // Bottom-right corner lands on the last row/column of the grid, even
        // though the pointer itself may be on the status bar: the highlight the
        // user sees ends at the grid's last cell.
        assert_eq!(
            cell_at(width - 1, height - 1, cols, rows),
            (cols - 1, rows - 1),
            "{width}x{height}: bottom-right corner"
        );
        // Every sampled pointer position is inside the model.
        for y in (0..height).step_by(97) {
            for x in (0..width).step_by(101) {
                let (col, row) = cell_at(x, y, cols, rows);
                assert!(col < cols && row < rows, "{width}x{height}: ({x},{y})");
            }
        }
        // A drag purely on the status bar still addresses the last text row
        // instead of a row that does not exist (which `read_row` used to
        // substitute with the last row, duplicating it on copy).
        assert_eq!(cell_at(0, height - 1, cols, rows).1, rows - 1);
    }
}

#[test]
fn cell_at_keeps_the_column_inside_a_narrow_grid() {
    // A grid narrower than the screen (e.g. a caller with a smaller model) must
    // clamp too, so the host property holds for any grid, not just the derived one.
    assert_eq!(cell_at(usize::MAX, usize::MAX, 10, 5), (9, 4));
    assert_eq!(cell_at(0, 0, 10, 5), (0, 0));
    assert_eq!(cell_at(CHAR_WIDTH * 4 + 3, CHAR_HEIGHT * 2 + 7, 10, 5), (4, 2));
}