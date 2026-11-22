//! Per-second frame accounting for the `--frame-stats FILE` diagnostic.
//!
//! The compositor's existing `--frame-log-every N` line answers "how expensive
//! were the last N frames"; it cannot answer "was the compositor *running* at
//! second T while another kernel task was busy" — the line only exists once N
//! frames have arrived, so a starved compositor produces no line at all. That is
//! exactly the question a freeze report asks, so this bucket is driven by the
//! main loop's clock, not by frame count: a second with zero presented frames
//! still emits `frames=0`, and the gap between two `t=` values is the length of
//! the unscanned interval.
//!
//! The accounting/formatting half is pure (`core`-free but IO-free) and
//! host-tested; `server.rs` owns the file handle and the wall clock.

use crate::timing::Samples;

/// One reporting interval (nominally one second) of frame statistics.
#[derive(Debug)]
pub struct FrameBucket {
    samples: Samples,
    frames: u64,
    damage: u64,
    gap_max_ms: f64,
    /// Сколько evdev-событий прочитано к моменту строки (живость ввода).
    ev_events: u64,
    /// Позиция указателя композитора на момент строки.
    ptr: (i32, i32),
}

impl FrameBucket {
    /// An empty bucket with room for ~2 seconds of 120 Hz frames.
    pub fn new() -> Self {
        FrameBucket {
            samples: Samples::with_capacity(256),
            frames: 0,
            damage: 0,
            gap_max_ms: 0.0,
            ev_events: 0,
            ptr: (0, 0),
        }
    }

    /// Record one presented frame and the number of damage rects it carried.
    pub fn record(&mut self, frame_ms: f64, damage_rects: u64) {
        self.samples.push(frame_ms);
        self.frames += 1;
        self.damage += damage_rects;
    }

    /// Record the wall time since the previous presented frame.
    ///
    /// This is the starvation metric: a long `gap_max_ms` with a small
    /// `avg_ms` means the compositor was not *scheduled*, not that a frame was
    /// expensive to draw.
    pub fn note_gap(&mut self, gap_ms: f64) {
        if gap_ms > self.gap_max_ms {
            self.gap_max_ms = gap_ms;
        }
    }

    /// Снимок ввода: сколько evdev-событий прочитано и где указатель.
    ///
    /// Строка должна отвечать не только «рисовали ли», но и «дошёл ли ввод»:
    /// курсор, который не двигается при живых кадрах, — это не голодание
    /// рендера, и без этого поля два случая неразличимы по логу.
    pub fn set_input(&mut self, ev_events: u64, ptr: (i32, i32)) {
        self.ev_events = ev_events;
        self.ptr = ptr;
    }

    /// Longest interval between two presented frames in this bucket.
    pub fn gap_max_ms(&self) -> f64 {
        self.gap_max_ms
    }

    /// Average frame time (ms) of the presented frames in this bucket.
    pub fn avg_ms(&self) -> f64 {
        self.samples.avg()
    }

    /// Longest single frame (ms) in this bucket.
    pub fn max_ms(&self) -> f64 {
        self.samples.max()
    }

    /// Frames recorded in this interval (0 means the compositor did not
    /// present a single frame — the starvation signal).
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Serialize the interval as one line (`t` is seconds since server start).
    ///
    /// The exact shape is pinned by host tests so a log consumer can parse it:
    /// `t=<s> frames=<n> avg_ms=<a> p95_ms=<p> max_ms=<m> gap_max_ms=<g>
    /// damage=<d>`.
    pub fn line(&self, t: f64) -> String {
        format!(
            "t={:.1} frames={} avg_ms={:.3} p95_ms={:.3} max_ms={:.3} gap_max_ms={:.1} \
             damage={} ev={} ptr={},{}",
            t,
            self.frames,
            self.samples.avg(),
            self.samples.p95(),
            self.samples.max(),
            self.gap_max_ms,
            self.damage,
            self.ev_events,
            self.ptr.0,
            self.ptr.1
        )
    }

    /// Reset for the next interval, keeping the sample capacity.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.frames = 0;
        self.damage = 0;
        self.gap_max_ms = 0.0;
        // `ev`/`ptr` — снимок на момент строки, а не накопление: их не сбрасываем,
        // иначе строка перестанет показывать, что ввод вообще читался.
    }
}

impl Default for FrameBucket {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bucket_reports_zero_frames() {
        // The whole point of the file: a frozen compositor must be visible as a
        // zero-frame second, not as a missing line.
        let bucket = FrameBucket::new();
        assert_eq!(bucket.frames(), 0);
        assert_eq!(
            bucket.line(3.0),
            "t=3.0 frames=0 avg_ms=0.000 p95_ms=0.000 max_ms=0.000 gap_max_ms=0.0 \
             damage=0 ev=0 ptr=0,0"
        );
    }

    #[test]
    fn records_frames_and_aggregates_frame_time() {
        let mut bucket = FrameBucket::new();
        for ms in [1.0, 2.0, 3.0, 4.0, 10.0] {
            bucket.record(ms, 2);
        }
        assert_eq!(bucket.frames(), 5);
        let line = bucket.line(12.5);
        assert!(line.starts_with("t=12.5 frames=5 "), "{line}");
        assert!(line.contains("avg_ms=4.000"), "{line}");
        assert!(line.contains("max_ms=10.000"), "{line}");
        // nearest-rank p95 of 5 samples: floor(0.95 * 4) = index 3 -> 4.0 ms
        assert!(line.contains("p95_ms=4.000"), "{line}");
        assert!(line.contains("gap_max_ms=0.0"), "{line}");
        assert!(line.contains("damage=10"), "{line}");
        // A live-input snapshot is part of the line, so a log reader can tell a
        // starved renderer from lost input.
        bucket.set_input(1234, (640, 400));
        let line = bucket.line(12.5);
        assert!(line.ends_with("ev=1234 ptr=640,400"), "{line}");
        bucket.note_gap(42.5);
        bucket.note_gap(7.0);
        assert_eq!(bucket.gap_max_ms(), 42.5);
        assert!(
            bucket.line(14.0).contains("gap_max_ms=42.5"),
            "{}",
            bucket.line(14.0)
        );
        bucket.clear();
        assert_eq!(bucket.frames(), 0);
        assert!(bucket.line(13.0).contains("frames=0"));
    }
}
