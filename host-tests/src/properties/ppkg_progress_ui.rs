// Feature: `ppkg install` progress UI — deterministic renderer snapshots.
//
// The effectful console driver in `src/shell/progress.rs` only decides *where*
// bytes go; every visual rule lives in `src/shell/ppkg_progress.rs`, which is
// `core` + `alloc` only and included here by `#[path]`. These tests pin:
//
//   * the known bar `[====    ]` and its exact fill at several ratios,
//   * the unknown-total `<=>` animation position
//     `(elapsed_ms / 500) % (inner_width - 4)` at t = 0/250/500/750/1000 ms,
//   * percent/speed/size/ETA widgets (integer math; no floats in the kernel),
//   * the 10 FPS redraw gate,
//   * the ANSI multi-line repaint sequence (`ESC[?25l`, `ESC[nA\rESC[K`,
//     `ESC[?25h`) and the dumb-console carriage-return fallback,
//   * the non-interactive/plain fallback emitting no escape bytes,
//   * completed bars printed exactly once and dropped from the live frame,
//   * panic-freedom on absurd values.
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::ppkg_progress::{
    eta_field, human_bytes, known_bar, percent_field, plain_phase_line, plain_plan_line,
    speed_and_eta, unknown_bar, unknown_marker_pos, AnsiEmitter, CrEmitter, Phase, ProgressMode,
    ProgressModel, BAR_WIDTH, REDRAW_INTERVAL_MS,
};

fn plan() -> Vec<(String, u64)> {
    vec![
        ("alpha".to_string(), 3 * 1024 * 1024),
        ("beta".to_string(), 1024 * 1024),
    ]
}

// ─────────────────────────── bar geometry ───────────────────────────

#[test]
fn known_bar_fill_is_exact() {
    // Width includes the two brackets.
    assert_eq!(known_bar(12, 0, 10), "[          ]");
    assert_eq!(known_bar(12, 5, 10), "[=====     ]");
    assert_eq!(known_bar(12, 10, 10), "[==========]");
    assert_eq!(known_bar(12, 11, 10), "[==========]"); // clamped
    assert_eq!(known_bar(12, 0, 0), "[          ]"); // no divide-by-zero
    // A width too small for a bar still yields a well-formed, panic-free string.
    assert_eq!(known_bar(0, 1, 2), "[= ]"); // width clamped to 4 -> inner 2
    assert_eq!(known_bar(4, 1, 2), "[= ]");
    assert_eq!(known_bar(BAR_WIDTH, 0, 1).chars().count(), BAR_WIDTH);
}

#[test]
fn unknown_marker_animation_times() {
    // inner width 26 -> span 22, so the 500 ms step is exact and visible.
    assert_eq!(unknown_marker_pos(26, 0), 0);
    assert_eq!(unknown_marker_pos(26, 250), 0);
    assert_eq!(unknown_marker_pos(26, 500), 1);
    assert_eq!(unknown_marker_pos(26, 750), 1);
    assert_eq!(unknown_marker_pos(26, 1000), 2);
    assert_eq!(unknown_marker_pos(26, 10_500), 21 % 22);
    // Degenerate widths never index out of bounds.
    assert_eq!(unknown_marker_pos(4, 5000), 0);
    assert_eq!(unknown_marker_pos(0, 5000), 0);
}

#[test]
fn unknown_bar_snapshots_at_0_250_500_750_1000() {
    assert_eq!(unknown_bar(28, 0), "[<=>                       ]");
    assert_eq!(unknown_bar(28, 250), "[<=>                       ]");
    assert_eq!(unknown_bar(28, 500), "[ <=>                      ]");
    assert_eq!(unknown_bar(28, 750), "[ <=>                      ]");
    assert_eq!(unknown_bar(28, 1000), "[  <=>                     ]");
    for t in [0u64, 250, 500, 750, 1000, 60_000] {
        let bar = unknown_bar(BAR_WIDTH, t);
        assert_eq!(bar.chars().count(), BAR_WIDTH);
        assert_eq!(bar.chars().filter(|c| *c == '<').count(), 1);
        assert_eq!(bar.chars().filter(|c| *c == '=').count(), 1);
        assert_eq!(bar.chars().filter(|c| *c == '>').count(), 1);
    }
}

// ─────────────────────────── widgets ───────────────────────────

#[test]
fn percent_field_is_four_columns() {
    assert_eq!(percent_field(0, 100, true), "  0%");
    assert_eq!(percent_field(42, 100, true), " 42%");
    assert_eq!(percent_field(100, 100, true), "100%");
    assert_eq!(percent_field(200, 100, true), "100%");
    assert_eq!(percent_field(50, 100, false), "???%");
    assert_eq!(percent_field(0, 0, true), "???%");
}

#[test]
fn human_bytes_units() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(1536), "1.5 KiB");
    assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    assert_eq!(human_bytes(10 * 1024 * 1024 + 512 * 1024), "10.5 MiB");
}

#[test]
fn speed_and_eta_use_integer_math() {
    // 2 MiB in 2 s = 1.0 MiB/s; 2 MiB remaining -> 2 s.
    let mib = 1024 * 1024;
    let (speed, eta) = speed_and_eta(2 * mib, 4 * mib, 2000);
    assert_eq!(speed, "1.0 MiB/s");
    assert_eq!(eta, "00:02");
    // Zero elapsed / zero done: unknown widgets, never a division by zero.
    assert_eq!(speed_and_eta(0, 100, 0), ("--.-- MiB/s".to_string(), "--:--".to_string()));
    assert_eq!(speed_and_eta(0, 100, 50), ("--.-- MiB/s".to_string(), "--:--".to_string()));
    // Complete: ETA 00:00, speed still reported.
    let (_, eta) = speed_and_eta(4 * mib, 4 * mib, 2000);
    assert_eq!(eta, "00:00");
    assert_eq!(eta_field(65_000), "01:05");
    assert_eq!(eta_field(3_600_000), "60:00");
}

// ─────────────────────────── the model ───────────────────────────

#[test]
fn progress_mode_parses_the_cli_values() {
    assert_eq!(ProgressMode::parse("ansi"), Some(ProgressMode::Ansi));
    assert_eq!(ProgressMode::parse("cr"), Some(ProgressMode::CarriageReturn));
    assert_eq!(ProgressMode::parse("carriage-return"), Some(ProgressMode::CarriageReturn));
    assert_eq!(ProgressMode::parse("plain"), Some(ProgressMode::Plain));
    assert_eq!(ProgressMode::parse("quiet"), Some(ProgressMode::Plain));
    assert_eq!(ProgressMode::parse("nope"), None);
    assert_eq!(ProgressMode::parse(""), None);
    assert!(ProgressMode::Plain.is_live() == false);
    assert!(ProgressMode::Ansi.is_live());
    assert!(ProgressMode::CarriageReturn.is_live());
}

#[test]
fn ten_fps_redraw_gate() {
    let mut model = ProgressModel::new(ProgressMode::Ansi, 1000);
    assert!(!model.redraw_due(1000), "same instant must not redraw");
    assert!(!model.redraw_due(1000 + REDRAW_INTERVAL_MS - 1));
    assert!(model.redraw_due(1000 + REDRAW_INTERVAL_MS));
    assert!(!model.redraw_due(1000 + REDRAW_INTERVAL_MS + 99));
    assert!(model.redraw_due(1000 + 2 * REDRAW_INTERVAL_MS));
}

#[test]
fn download_frame_snapshot_at_zero() {
    let mut model = ProgressModel::new(ProgressMode::Ansi, 0);
    model.plan(&plan());
    let lines = model.frame(0);
    assert_eq!(lines.len(), 3, "active bar + divider + total: {:?}", lines);
    // Exact first line: name column, unknown bar at marker 0, ???%, phase size.
    assert_eq!(
        lines[0],
        "  Installing alpha                [<=>                       ] ???%  3.0 MiB (download)  --.-- MiB/s  ETA --:--"
    );
    // Exact total line: known bar (0 of 4 MiB) and a real 0%.
    assert_eq!(
        lines[2],
        "  Total                   [                          ]   0%  0 B / 4.0 MiB  --.-- MiB/s  ETA --:--"
    );
    assert!(lines[1].starts_with("  -----"));
}

#[test]
fn known_unpack_frame_snapshot_at_snapshots() {
    // t = 0/250/500/750/1000 ms must be deterministic and monotone.
    let mib = 1024 * 1024;
    let mut model = ProgressModel::new(ProgressMode::CarriageReturn, 0);
    model.plan(&plan());
    model.begin_phase("alpha", Phase::Unpack);
    model.unpack_progress("alpha", mib, 3 * mib);
    let mut bars = Vec::new();
    for t in [0u64, 250, 500, 750, 1000] {
        let lines = model.frame(t);
        assert_eq!(lines.len(), 3);
        // 1/3 of the inner width (26/3 = 8 cells), 33%, size and ETA.
        let expected_bar = crate::ppkg_progress::known_bar(BAR_WIDTH, mib, 3 * mib);
        assert_eq!(expected_bar, "[========                  ]");
        assert!(lines[0].contains(&expected_bar), "{}", lines[0]);
        assert!(lines[0].contains(" 33%"), "{}", lines[0]);
        assert!(lines[0].contains("1.0 MiB / 3.0 MiB"), "{}", lines[0]);
        assert!(lines[0].contains("ETA "), "{}", lines[0]);
        // Speed is only defined once some time has elapsed.
        if t > 0 {
            assert!(lines[0].contains("MiB/s"), "{}", lines[0]);
        }
        assert!(lines[0].starts_with("  Installing alpha"), "{}", lines[0]);
        bars.push(lines[0].clone());
    }
    // Same t twice -> identical frame (pure function).
    assert_eq!(model.frame(500), model.frame(500));
}

#[test]
fn completed_bar_prints_once_and_leaves_the_frame() {
    let mut model = ProgressModel::new(ProgressMode::Ansi, 0);
    model.plan(&plan());
    let first = model.complete_package("alpha", 12).expect("first completion");
    assert_eq!(first, "  [ok] alpha (12 files, 3.0 MiB)");
    assert!(model.complete_package("alpha", 12).is_none(), "printed exactly once");
    assert_eq!(model.percent_done(), 75);
    // The live frame now tracks the next package, not the finished one.
    let lines = model.frame(100);
    assert!(lines[0].contains("beta"), "{:?}", lines);
    assert!(!lines[0].contains("alpha"), "{:?}", lines);
    model.complete_package("beta", 1);
    assert_eq!(model.percent_done(), 100);
    assert_eq!(model.frame(100).len(), 2, "divider + total only");
}

#[test]
fn plain_mode_emits_plain_lines_without_escape_bytes() {
    let mut model = ProgressModel::new(ProgressMode::Plain, 0);
    model.plan(&plan());
    assert!(model.frame(0).is_empty(), "plain mode has no live frame");
    let plan_line = plain_plan_line(2, 4 * 1024 * 1024);
    assert_eq!(plan_line, "ppkg: installing 2 package(s), 4.0 MiB to download");
    let unpack = plain_phase_line("alpha", Phase::Unpack, 3 * 1024 * 1024).expect("unpack line");
    assert_eq!(unpack, "ppkg: unpacking alpha (3.0 MiB)");
    assert!(plain_phase_line("alpha", Phase::Download, 1).is_none());
    assert!(plain_phase_line("alpha", Phase::Verify, 1).is_none());
    for line in [plan_line, unpack] {
        assert!(!line.contains('\x1b'), "plain output must not carry ANSI: {:?}", line);
        assert!(!line.contains('\r'));
    }
}

#[test]
fn model_handles_absurd_and_empty_input_without_panic() {
    let mut empty = ProgressModel::new(ProgressMode::Ansi, 0);
    empty.plan(&[]);
    assert!(empty.frame(0).len() <= 2);
    assert_eq!(empty.percent_done(), 0);
    empty.complete_package("ghost", 0); // unknown package: no-op
    empty.begin_phase("ghost", Phase::Unpack);
    empty.unpack_progress("ghost", 1, 2);

    let huge = vec![("japanese-日本語-package".to_string(), u64::MAX / 2)];
    let mut model = ProgressModel::new(ProgressMode::CarriageReturn, 0);
    model.plan(&huge);
    model.begin_phase("japanese-日本語-package", Phase::Unpack);
    model.unpack_progress("japanese-日本語-package", u64::MAX / 4, u64::MAX);
    let lines = model.frame(u64::MAX / 2);
    assert_eq!(lines.len(), 3);
    assert!(percent_field(u64::MAX, u64::MAX, true).starts_with("100%"));
}

// ─────────────────────────── emitters ───────────────────────────

#[test]
fn ansi_emitter_sequence_is_exact() {
    let mut e = AnsiEmitter::new();
    assert_eq!(e.begin(), "\x1b[?25l");
    assert_eq!(e.begin(), "", "cursor hidden exactly once");

    let first = e.frame(&["one".to_string(), "two".to_string()]);
    assert_eq!(first, "\r\x1b[Kone\n\r\x1b[Ktwo\n");
    assert_eq!(e.lines(), 2);

    // Shorter frame: move up over both, clear both, pad the missing line.
    let second = e.frame(&["ONE".to_string()]);
    assert_eq!(
        second,
        "\x1b[2A\r\x1b[KONE\n\r\x1b[K\n",
        "must move up 2 lines and clear both"
    );
    assert_eq!(e.lines(), 2);

    // Longer frame: move up, redraw, then append the extra lines.
    let third = e.frame(&["a".to_string(), "b".to_string(), "c".to_string()]);
    assert_eq!(third, "\x1b[2A\r\x1b[Ka\n\r\x1b[Kb\n\r\x1b[Kc\n");
    assert_eq!(e.lines(), 3);

    let done = e.finish(&["[ok] all".to_string()]);
    assert_eq!(
        done,
        "\x1b[3A\r\x1b[K\n\r\x1b[K\n\r\x1b[K\n[ok] all\n\x1b[?25h"
    );
    assert_eq!(e.lines(), 0);
}

#[test]
fn cr_emitter_pads_shorter_redraws_and_prints_completed_once() {
    let mut e = CrEmitter::new();
    assert_eq!(e.live("abc"), "\rabc");
    assert_eq!(e.live("xy"), "\rxy "); // one trailing space erases 'c'
    let perm = e.permanent("[ok] alpha");
    assert_eq!(perm, "\r   \r\n[ok] alpha\n");
    // After a permanent line the live width is reset: no stale padding.
    assert_eq!(e.live("z"), "\rz");
}

#[test]
fn ansi_and_cr_emitters_never_emit_from_plain_mode() {
    // Plain mode is represented by the model returning no frames; the driver then
    // writes only plain lines. This test documents the contract at the model level.
    let mut model = ProgressModel::new(ProgressMode::Plain, 0);
    model.plan(&plan());
    model.begin_phase("alpha", Phase::Unpack);
    model.unpack_progress("alpha", 1, 2);
    assert!(model.frame(1_000).is_empty());
    assert!(known_bar(BAR_WIDTH, 1, 2).chars().all(|c| c != '\x1b'));
}
