//! Streaming UTF-8 + VT100/ANSI parser.
//!
//! The parser is a byte-at-a-time state machine.  It never panics on malformed
//! input: unknown sequences, truncated strings and invalid UTF-8 are consumed
//! or replaced with U+FFFD.  Terminal *semantics* (cursor, grid, modes) live in
//! [`Screen`]; this module only decodes escape sequences and calls it.
//!
//! Coverage:
//!
//! * C0 controls, SO/SI, DEC special graphics and UK charset designations.
//! * CSI: cursor movement, erase, insert/delete, scroll region, tabs, SGR
//!   (16/256/truecolor, attributes, colon sub-parameters), mode set/reset,
//!   device status/attributes, DECRQM, window size reports, cursor style.
//! * OSC: title (0/2), color queries (10/11), palette/hyperlink/ignore.
//! * DCS/SOS/PM/APC: consumed through ST without leaking to the grid.
//! * UTF-8 incremental decoding across `feed()` calls, Unicode width classes.
//!
//! All replies (DA, CPR, DECRQM, ...) are queued in [`Terminal::take_responses`]
//! so the caller can write them back to the PTY / kernel VT input.

use crate::cell::{Attrs, Color, Pen};
use crate::screen::{CursorStyle, Screen};

const MAX_PARAMS: usize = 32;
const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC: usize = 8192;
const MAX_TITLE: usize = 1024;

/// A streaming terminal: screen state plus parser state.
pub struct Terminal {
    pub screen: Screen,
    parser: Parser,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Terminal {
        Terminal {
            screen: Screen::new(cols, rows),
            parser: Parser::new(),
        }
    }

    pub fn with_scrollback(cols: usize, rows: usize, max_scrollback: usize) -> Terminal {
        Terminal {
            screen: Screen::with_scrollback(cols, rows, max_scrollback),
            parser: Parser::new(),
        }
    }

    /// Feed arbitrary bytes (terminal output, PTY reads, script files).
    pub fn feed(&mut self, data: &[u8]) {
        self.parser.feed(data, &mut self.screen);
    }

    /// Feed a string.  Useful for tests and scripts.
    pub fn feed_str(&mut self, data: &str) {
        self.feed(data.as_bytes());
    }

    /// Drain replies produced by queries (DA, CPR, DECRQM, ...).
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.responses)
    }

    pub fn responses(&self) -> &[u8] {
        &self.parser.responses
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }

    /// Flush an incomplete UTF-8 sequence as U+FFFD.  Normal streaming use does
    /// not need this; it is for "the producer is gone" cases at shutdown.
    pub fn finish(&mut self) {
        if self.parser.utf8.need > 0 {
            self.parser.utf8.reset();
            self.screen.write_char('\u{FFFD}');
        }
    }
}

impl std::ops::Deref for Terminal {
    type Target = Screen;

    fn deref(&self) -> &Screen {
        &self.screen
    }
}

// ── parameter list ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct Params {
    values: [u32; MAX_PARAMS],
    /// Separator *before* each value: 0 for the first, `b';'` or `b':'`.
    seps: [u8; MAX_PARAMS],
    len: usize,
}

impl Params {
    const fn new() -> Params {
        Params {
            values: [0; MAX_PARAMS],
            seps: [0; MAX_PARAMS],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, sep: u8) {
        if self.len < MAX_PARAMS {
            self.values[self.len] = 0;
            self.seps[self.len] = sep;
            self.len += 1;
        }
    }

    fn ensure_value(&mut self) {
        if self.len == 0 {
            self.push(0);
        }
    }

    fn digit(&mut self, d: u8) {
        self.ensure_value();
        let last = self.len - 1;
        self.values[last] = self.values[last]
            .saturating_mul(10)
            .saturating_add(u32::from(d - b'0'));
    }

    fn separator(&mut self, sep: u8) {
        self.ensure_value();
        self.push(sep);
    }

    fn value(&self, i: usize) -> Option<u32> {
        if i < self.len {
            Some(self.values[i])
        } else {
            None
        }
    }

    /// CSI parameter with the customary 0/absent default.
    fn p(&self, i: usize, default: usize) -> usize {
        match self.value(i) {
            None | Some(0) => default,
            Some(v) => v as usize,
        }
    }

    fn raw(&self, i: usize) -> u32 {
        self.value(i).unwrap_or(0)
    }

    fn sep(&self, i: usize) -> u8 {
        if i < self.len {
            self.seps[i]
        } else {
            0
        }
    }
}

// ── UTF-8 decoder ──────────────────────────────────────────────────────────

struct Utf8Decoder {
    need: u8,
    cp: u32,
    min: u32,
    pending: Option<u8>,
}

enum Utf8Outcome {
    Char(char),
    Replacement,
    Incomplete,
}

impl Utf8Decoder {
    const fn new() -> Utf8Decoder {
        Utf8Decoder {
            need: 0,
            cp: 0,
            min: 0,
            pending: None,
        }
    }

    fn reset(&mut self) {
        self.need = 0;
        self.cp = 0;
        self.min = 0;
        self.pending = None;
    }

    fn push(&mut self, b: u8) -> Utf8Outcome {
        if self.need == 0 {
            if b < 0x80 {
                return Utf8Outcome::Char(b as char);
            }
            let (need, cp, min) = match b {
                0xC2..=0xDF => (1, u32::from(b & 0x1F), 0x80),
                0xE0..=0xEF => (2, u32::from(b & 0x0F), 0x800),
                0xF0..=0xF4 => (3, u32::from(b & 0x07), 0x1_0000),
                _ => return Utf8Outcome::Replacement,
            };
            self.need = need;
            self.cp = cp;
            self.min = min;
            return Utf8Outcome::Incomplete;
        }

        if b & 0xC0 != 0x80 {
            self.need = 0;
            self.cp = 0;
            if b < 0x80 {
                self.pending = Some(b);
            }
            return Utf8Outcome::Replacement;
        }
        self.cp = (self.cp << 6) | u32::from(b & 0x3F);
        self.need -= 1;
        if self.need > 0 {
            return Utf8Outcome::Incomplete;
        }
        let cp = self.cp;
        self.cp = 0;
        if cp < self.min || cp > 0x10_FFFF || (0xD800..=0xDFFF).contains(&cp) {
            Utf8Outcome::Replacement
        } else {
            match char::from_u32(cp) {
                Some(ch) => Utf8Outcome::Char(ch),
                None => Utf8Outcome::Replacement,
            }
        }
    }
}

// ── parser state ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    Osc,
    /// ESC was seen inside an OSC; only `\` finishes it.
    OscEsc,
    /// DCS/SOS/PM/APC payload; ignored until ST.
    String,
    StringEsc,
}

struct Parser {
    state: State,
    params: Params,
    private: u8,
    inter: [u8; MAX_INTERMEDIATES],
    ninter: usize,
    esc_inter: [u8; MAX_INTERMEDIATES],
    nesc: usize,
    osc: Vec<u8>,
    osc_overflow: bool,
    utf8: Utf8Decoder,
    responses: Vec<u8>,
}

impl Parser {
    fn new() -> Parser {
        Parser {
            state: State::Ground,
            params: Params::new(),
            private: 0,
            inter: [0; MAX_INTERMEDIATES],
            ninter: 0,
            esc_inter: [0; MAX_INTERMEDIATES],
            nesc: 0,
            osc: Vec::new(),
            osc_overflow: false,
            utf8: Utf8Decoder::new(),
            responses: Vec::new(),
        }
    }

    fn feed(&mut self, data: &[u8], screen: &mut Screen) {
        for &b in data {
            self.byte(b, screen);
        }
    }

    fn byte(&mut self, b: u8, screen: &mut Screen) {
        match self.state {
            State::Ground => self.ground(b, screen),
            State::Escape => self.escape(b, screen),
            State::Csi => self.csi_byte(b, screen),
            State::Osc => self.osc_byte(b, screen),
            State::OscEsc => self.osc_esc(b, screen),
            State::String => self.string_byte(b, screen),
            State::StringEsc => self.string_esc(b, screen),
        }
    }

    fn ground(&mut self, b: u8, screen: &mut Screen) {
        let mut byte = b;
        loop {
            if self.utf8.need > 0 || byte >= 0x80 {
                match self.utf8.push(byte) {
                    Utf8Outcome::Char(ch) => {
                        let mapped = screen.map_charset(ch);
                        screen.write_char(mapped);
                        return;
                    }
                    Utf8Outcome::Incomplete => return,
                    Utf8Outcome::Replacement => {
                        screen.write_char('\u{FFFD}');
                        if let Some(next) = self.utf8.pending.take() {
                            byte = next;
                            continue;
                        }
                        return;
                    }
                }
            }
            match byte {
                0x1B => {
                    self.state = State::Escape;
                    self.nesc = 0;
                }
                0x07 => screen.bell_count += 1,
                0x08 => screen.backspace(),
                0x09 => screen.tab_forward(1),
                0x0A | 0x0B | 0x0C => screen.linefeed(),
                0x0D => screen.carriage_return(),
                0x0E => screen.shift_charset(1),
                0x0F => screen.shift_charset(0),
                0x18 | 0x1A => {}
                0x00..=0x1F | 0x7F => {}
                _ => {
                    let ch = byte as char;
                    let mapped = screen.map_charset(ch);
                    screen.write_char(mapped);
                }
            }
            return;
        }
    }

    /// Execute a C0 control; used from inside escape/CSI states.
    fn control(&mut self, b: u8, screen: &mut Screen) {
        match b {
            0x07 => screen.bell_count += 1,
            0x08 => screen.backspace(),
            0x09 => screen.tab_forward(1),
            0x0A | 0x0B | 0x0C => screen.linefeed(),
            0x0D => screen.carriage_return(),
            0x0E => screen.shift_charset(1),
            0x0F => screen.shift_charset(0),
            _ => {}
        }
    }

    fn escape(&mut self, b: u8, screen: &mut Screen) {
        match b {
            0x1B => {
                self.nesc = 0;
            }
            0x18 | 0x1A => {
                self.state = State::Ground;
                self.nesc = 0;
            }
            0x00..=0x1F => {
                self.control(b, screen);
                self.state = State::Ground;
                self.nesc = 0;
            }
            0x20..=0x2F => {
                if self.nesc < MAX_INTERMEDIATES {
                    self.esc_inter[self.nesc] = b;
                    self.nesc += 1;
                }
            }
            0x30..=0x7E => {
                let inter = self.esc_inter;
                let nesc = self.nesc;
                self.state = State::Ground;
                self.nesc = 0;
                self.escape_dispatch(&inter[..nesc], b, screen);
            }
            _ => {}
        }
    }

    fn escape_dispatch(&mut self, inter: &[u8], final_byte: u8, screen: &mut Screen) {
        match (inter, final_byte) {
            ([], b'[') => {
                self.state = State::Csi;
                self.params.clear();
                self.private = 0;
                self.ninter = 0;
            }
            ([], b']') => {
                self.state = State::Osc;
                self.osc.clear();
                self.osc_overflow = false;
            }
            ([], b'P') | ([], b'X') | ([], b'^') | ([], b'_') => {
                self.state = State::String;
            }
            ([], b'7') => screen.save_cursor(),
            ([], b'8') => screen.restore_cursor(),
            ([], b'D') => screen.linefeed(),
            ([], b'E') => {
                screen.linefeed();
                screen.carriage_return();
            }
            ([], b'H') => screen.set_tabstop(),
            ([], b'M') => screen.reverse_index(),
            ([], b'c') => screen.reset(),
            ([], b'=') | ([], b'>') => {}
            ([], b'N') | ([], b'O') => {}
            ([], b'Z') => self.respond(b"\x1b[?62;22c"),
            ([], b'\\') => {}
            ([b'#'], b'8') => screen.fill_alignment(),
            ([b'('], designator) => screen.set_charset(0, designator),
            ([b')'], designator) => screen.set_charset(1, designator),
            ([b'*'], _) | ([b'+'], _) => {}
            ([b'%'], b'G') | ([b'%'], b'@') => {}
            ([b' '], b'F') | ([b' '], b'G') => {}
            _ => {}
        }
    }

    fn csi_byte(&mut self, b: u8, screen: &mut Screen) {
        match b {
            0x1B => {
                self.state = State::Escape;
                self.nesc = 0;
            }
            0x18 | 0x1A => {
                self.state = State::Ground;
                self.params.clear();
            }
            0x00..=0x1F => self.control(b, screen),
            0x20..=0x2F => {
                if self.ninter < MAX_INTERMEDIATES {
                    self.inter[self.ninter] = b;
                    self.ninter += 1;
                }
            }
            0x30..=0x39 => self.params.digit(b),
            0x3A | 0x3B => self.params.separator(b),
            0x3C..=0x3F => {
                if self.params.len == 0 && self.private == 0 {
                    self.private = b;
                }
            }
            0x40..=0x7E => {
                let inter = self.inter;
                let ninter = self.ninter;
                self.state = State::Ground;
                self.csi_dispatch(&inter[..ninter], b, screen);
            }
            0x7F => {}
            // A raw UTF-8 byte cannot appear inside a CSI; abort instead of
            // staying in the sequence and swallowing the rest of the stream.
            _ => self.state = State::Ground,
        }
    }

    fn csi_dispatch(&mut self, inter: &[u8], final_byte: u8, screen: &mut Screen) {
        if !inter.is_empty() {
            match (inter, final_byte) {
                ([b'$'], b'p') => self.report_mode(screen),
                ([b' '], b'q') => {
                    let style = match self.params.raw(0) {
                        0 | 1 | 2 => CursorStyle::Block,
                        3 | 4 => CursorStyle::Underline,
                        5 | 6 => CursorStyle::Bar,
                        _ => CursorStyle::Block,
                    };
                    screen.cursor.style = style;
                }
                ([b'$'], b'q') => {}
                ([b'"'], b'p') => {}
                ([b'!'], b'p') => self.report_mode(screen),
                _ => {}
            }
            return;
        }

        match self.private {
            b'?' => self.csi_private(final_byte, screen),
            b'>' => match final_byte {
                b'c' => self.respond(b"\x1b[>69;1;0c"),
                b'q' => self.respond(b"\x1bP>|PaghTerm 0.1\x1b\\"),
                b'm' | b'n' | b'h' | b'l' => {}
                _ => {}
            },
            b'=' | b'<' | b'!' => {}
            0 => self.csi_normal(final_byte, screen),
            _ => {}
        }
    }

    fn csi_private(&mut self, final_byte: u8, screen: &mut Screen) {
        match final_byte {
            b'h' | b'l' => {
                let enable = final_byte == b'h';
                for i in 0..self.params.len {
                    let mode = self.params.raw(i);
                    screen.set_mode(true, mode, enable);
                }
            }
            b's' => screen.save_private_modes_cmd(),
            b'r' => screen.restore_private_modes_cmd(),
            b'n' => {
                if self.params.raw(0) == 6 {
                    let report = format!("\x1b[?{};{}R", screen.cursor.y + 1, screen.cursor.x + 1);
                    self.respond(report.as_bytes());
                }
            }
            b'J' => screen.erase_in_display(self.params.raw(0)),
            b'K' => screen.erase_in_line(self.params.raw(0)),
            b'c' => self.respond(b"\x1b[?62;22c"),
            _ => {}
        }
    }

    fn csi_normal(&mut self, final_byte: u8, screen: &mut Screen) {
        match final_byte {
            b'@' => screen.insert_chars(self.params.p(0, 1)),
            b'A' => screen.move_up(self.params.p(0, 1)),
            b'B' => screen.move_down(self.params.p(0, 1)),
            b'C' => screen.move_right(self.params.p(0, 1)),
            b'D' => screen.move_left(self.params.p(0, 1)),
            b'E' => screen.next_line(self.params.p(0, 1)),
            b'F' => screen.previous_line(self.params.p(0, 1)),
            b'G' | b'`' => screen.set_column(self.params.p(0, 1) - 1),
            b'H' | b'f' => screen.goto(self.params.p(0, 1) - 1, self.params.p(1, 1) - 1),
            b'I' => screen.tab_forward(self.params.p(0, 1)),
            b'J' => screen.erase_in_display(self.params.raw(0)),
            b'K' => screen.erase_in_line(self.params.raw(0)),
            b'L' => screen.insert_lines(self.params.p(0, 1)),
            b'M' => screen.delete_lines(self.params.p(0, 1)),
            b'P' => screen.delete_chars(self.params.p(0, 1)),
            b'S' => screen.scroll_up(self.params.p(0, 1)),
            b'T' => screen.scroll_down(self.params.p(0, 1)),
            b'X' => screen.erase_chars(self.params.p(0, 1)),
            b'Z' => screen.tab_backward(self.params.p(0, 1)),
            b'a' => screen.move_right(self.params.p(0, 1)),
            b'b' => screen.repeat_last_char(self.params.p(0, 1)),
            b'c' => self.respond(b"\x1b[?62;22c"),
            b'd' => screen.set_row(self.params.p(0, 1) - 1),
            b'e' => screen.move_down(self.params.p(0, 1)),
            b'g' => screen.clear_tabstop(self.params.raw(0)),
            b'h' | b'l' => {
                let enable = final_byte == b'h';
                for i in 0..self.params.len {
                    let mode = self.params.raw(i);
                    screen.set_mode(false, mode, enable);
                }
            }
            b'm' => self.sgr(screen),
            b'n' => self.device_status(screen),
            b'q' => {}
            b'r' => {
                let top = self.params.p(0, 1) - 1;
                let bottom = self.params.p(1, screen.rows()) - 1;
                screen.set_scroll_region(top, bottom);
            }
            b's' => screen.save_cursor(),
            b'u' => screen.restore_cursor(),
            b't' => self.window_op(screen),
            b'x' => self.respond(b"\x1b[2;1;1;112;112;1x"),
            _ => {}
        }
    }

    fn report_mode(&mut self, screen: &Screen) {
        let private = self.private == b'?';
        let mode = self.params.raw(0);
        let state = screen.mode_state(private, mode).unwrap_or(0);
        let marker = if private { "?" } else { "" };
        let report = format!("\x1b[{marker}{mode};{state}$y");
        self.respond(report.as_bytes());
    }

    fn device_status(&mut self, screen: &mut Screen) {
        match self.params.raw(0) {
            5 => self.respond(b"\x1b[0n"),
            6 => {
                let report = format!("\x1b[{};{}R", screen.cursor.y + 1, screen.cursor.x + 1);
                self.respond(report.as_bytes());
            }
            _ => {}
        }
    }

    fn window_op(&mut self, screen: &mut Screen) {
        match self.params.raw(0) {
            18 => {
                let report = format!("\x1b[8;{};{}t", screen.rows(), screen.cols());
                self.respond(report.as_bytes());
            }
            14 => {
                let report = format!(
                    "\x1b[4;{};{}t",
                    screen.rows() * crate::font::CELL_H,
                    screen.cols() * crate::font::CELL_W
                );
                self.respond(report.as_bytes());
            }
            _ => {}
        }
    }

    fn sgr(&mut self, screen: &mut Screen) {
        if self.params.len == 0 {
            screen.reset_pen();
            return;
        }
        let mut pen: Pen = screen.pen;
        let mut i = 0;
        while i < self.params.len {
            let v = self.params.raw(i);
            match v {
                0 => pen = Pen::default(),
                1 => pen.attrs.insert(Attrs::BOLD),
                2 => pen.attrs.insert(Attrs::DIM),
                3 => pen.attrs.insert(Attrs::ITALIC),
                4 => {
                    pen.attrs.insert(Attrs::UNDERLINE);
                    // 4:3 (curly) and friends: consume the style subparameter.
                    if self.params.sep(i + 1) == b':' {
                        i += 1;
                    }
                }
                5 | 6 => pen.attrs.insert(Attrs::BLINK),
                7 => pen.attrs.insert(Attrs::REVERSE),
                8 => pen.attrs.insert(Attrs::HIDDEN),
                9 => pen.attrs.insert(Attrs::STRIKETHROUGH),
                21 => pen.attrs.insert(Attrs::UNDERLINE),
                22 => {
                    pen.attrs.remove(Attrs::BOLD);
                    pen.attrs.remove(Attrs::DIM);
                }
                23 => pen.attrs.remove(Attrs::ITALIC),
                24 => pen.attrs.remove(Attrs::UNDERLINE),
                25 => pen.attrs.remove(Attrs::BLINK),
                27 => pen.attrs.remove(Attrs::REVERSE),
                28 => pen.attrs.remove(Attrs::HIDDEN),
                29 => pen.attrs.remove(Attrs::STRIKETHROUGH),
                30..=37 => pen.fg = Color::Indexed((v - 30) as u8),
                38 | 48 => {
                    if let Some((color, used)) = self.extended_color(i) {
                        if v == 38 {
                            pen.fg = color;
                        } else {
                            pen.bg = color;
                        }
                        i += used;
                    }
                }
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Indexed((v - 40) as u8),
                49 => pen.bg = Color::Default,
                53 => pen.attrs.insert(Attrs::OVERLINE),
                55 => pen.attrs.remove(Attrs::OVERLINE),
                90..=97 => pen.fg = Color::Indexed((v - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Indexed((v - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
        screen.set_pen(pen);
    }

    /// Parse the payload of SGR 38/48.  Returns the color and how many extra
    /// parameters were consumed.  Handles `;` and `:` forms, including the
    /// empty colorspace slot emitted by some applications (`38:2::r:g:b`).
    fn extended_color(&self, i: usize) -> Option<(Color, usize)> {
        let mode = self.params.value(i + 1)?;
        match mode {
            5 => {
                let index = self.params.value(i + 2)? as u8;
                Some((Color::Indexed(index), 2))
            }
            2 => {
                let colon_colorspace = self.params.sep(i + 2) == b':';
                let has_extra = self.params.value(i + 5).is_some();
                let base = if colon_colorspace && has_extra {
                    i + 3
                } else {
                    i + 2
                };
                let r = self.params.value(base)?;
                let g = self.params.value(base + 1)?;
                let b = self.params.value(base + 2)?;
                let used = base + 3 - i - 1;
                Some((
                    Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8),
                    used,
                ))
            }
            _ => None,
        }
    }

    // ── OSC / DCS ─────────────────────────────────────────────────────────

    fn osc_byte(&mut self, b: u8, screen: &mut Screen) {
        match b {
            0x07 => {
                self.finish_osc(screen);
                self.state = State::Ground;
            }
            0x1B => {
                self.state = State::OscEsc;
            }
            0x18 | 0x1A => {
                self.osc.clear();
                self.osc_overflow = false;
                self.state = State::Ground;
            }
            _ => {
                if self.osc.len() < MAX_OSC {
                    self.osc.push(b);
                } else {
                    self.osc_overflow = true;
                }
            }
        }
    }

    fn osc_esc(&mut self, b: u8, screen: &mut Screen) {
        match b {
            b'\\' => {
                self.finish_osc(screen);
                self.state = State::Ground;
            }
            0x18 | 0x1A => {
                self.osc.clear();
                self.osc_overflow = false;
                self.state = State::Ground;
            }
            _ => {
                // Malformed ESC inside OSC: abandon the string.
                self.osc.clear();
                self.osc_overflow = false;
                self.state = State::Ground;
            }
        }
    }

    fn finish_osc(&mut self, screen: &mut Screen) {
        let buf = std::mem::take(&mut self.osc);
        let overflow = std::mem::replace(&mut self.osc_overflow, false);
        if overflow {
            return;
        }
        let text = String::from_utf8_lossy(&buf);
        let (command, rest) = match text.split_once(';') {
            Some((cmd, rest)) => (cmd.trim(), rest),
            None => (text.trim(), ""),
        };
        match command {
            "0" | "2" => {
                let title: String = rest
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_TITLE)
                    .collect();
                screen.set_title(title);
            }
            "10" | "11" if rest.starts_with('?') => {
                let crate::cell::Rgb(r, g, b) = if command == "10" {
                    screen.default_fg_rgb
                } else {
                    screen.default_bg_rgb
                };
                let report = format!(
                    "\x1b]{command};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                    u16::from(r) * 257,
                    u16::from(g) * 257,
                    u16::from(b) * 257
                );
                self.respond(report.as_bytes());
            }
            // OSC 4 palette changes, OSC 8 hyperlinks, shell-integration
            // markers and clipboard commands are parsed but intentionally not
            // acted on (see TERMINAL.md for the security rationale).
            _ => {}
        }
    }

    fn string_byte(&mut self, b: u8, _screen: &mut Screen) {
        match b {
            0x1B => self.state = State::StringEsc,
            0x18 | 0x1A => self.state = State::Ground,
            _ => {}
        }
    }

    fn string_esc(&mut self, b: u8, _screen: &mut Screen) {
        match b {
            b'\\' => self.state = State::Ground,
            0x18 | 0x1A => self.state = State::Ground,
            _ => {}
        }
    }

    fn respond(&mut self, bytes: &[u8]) {
        if self.responses.len() + bytes.len() <= 64 * 1024 {
            self.responses.extend_from_slice(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cols: usize, rows: usize, input: &str) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        t.feed_str(input);
        t
    }

    #[test]
    fn utf8_split_across_feeds() {
        let mut t = Terminal::new(10, 2);
        t.feed(&[0xD0]);
        assert_eq!(t.text(), "");
        t.feed(&[0x9F]);
        t.feed(&[0xD1, 0x80]);
        assert_eq!(t.text(), "Пр");
    }

    #[test]
    fn invalid_utf8_becomes_replacement_and_does_not_eat_ascii() {
        let mut t = Terminal::new(10, 2);
        t.feed(&[0xE2, 0x28, b'A']);
        assert_eq!(t.text(), "\u{FFFD}(A");
    }

    #[test]
    fn cursor_moves() {
        let mut t = run(10, 5, "\x1b[2;3H");
        assert_eq!((t.cursor.x, t.cursor.y), (2, 1));
        t.feed_str("\x1b[2A\x1b[3D");
        assert_eq!((t.cursor.x, t.cursor.y), (0, 0));
        t.feed_str("\x1b[5G\x1b[4d");
        assert_eq!((t.cursor.x, t.cursor.y), (4, 3));
    }

    #[test]
    fn sgr_color_forms() {
        let mut t = run(10, 2, "\x1b[31;44mA\x1b[38;5;196mB\x1b[38;2;12;34;56mC");
        assert_eq!(t.screen.cell(0, 0).fg, Color::Indexed(1));
        assert_eq!(t.screen.cell(0, 0).bg, Color::Indexed(4));
        assert_eq!(t.screen.cell(1, 0).fg, Color::Indexed(196));
        assert_eq!(t.screen.cell(2, 0).fg, Color::Rgb(12, 34, 56));
        t.feed_str("\x1b[38:2::1:2:3mD");
        assert_eq!(t.screen.cell(3, 0).fg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn dec_graphics_and_charset_reset() {
        let t = run(10, 2, "\x1b(0lqk\x1b(Bx");
        assert_eq!(t.text(), "┌─┐x");
    }

    #[test]
    fn alt_screen_restores_primary() {
        let mut t = run(10, 2, "main\x1b[?1049h alt\x1b[?1049l");
        assert_eq!(t.text(), "main");
        t.feed_str("\x1b[?1049h");
        t.feed_str("\x1b[2J\x1b[Halt");
        assert_eq!(t.text(), "alt");
        t.feed_str("\x1b[?1049l");
        assert_eq!(t.text(), "main");
    }

    #[test]
    fn scroll_region_keeps_rows_outside() {
        let t = run(6, 5, "top\x1b[2;4r\x1b[2;1HA\x1b[3;1HB\x1b[4;1HC\r\nD");
        assert_eq!(t.line_text(0), "top");
        assert_eq!(t.line_text(1), "B");
        assert_eq!(t.line_text(2), "C");
        assert_eq!(t.line_text(3), "D");
        assert_eq!(t.line_text(4), "");
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn queries_are_queued() {
        let mut t = run(20, 5, "\x1b[2;3H\x1b[6n\x1b[c");
        let responses = t.take_responses();
        let s = String::from_utf8_lossy(&responses);
        assert!(s.contains("\x1b[2;3R"), "{s:?}");
        assert!(s.contains("\x1b[?62;22c"), "{s:?}");
    }

    #[test]
    fn osc_title_bel_and_st() {
        let t = run(10, 2, "\x1b]0;one\x07\x1b]2;two\x1b\\");
        assert_eq!(t.screen.title, "two");
    }

    #[test]
    fn osc_overflow_is_ignored() {
        let mut t = Terminal::new(10, 2);
        t.feed(b"\x1b]0;");
        for _ in 0..MAX_OSC + 10 {
            t.feed(b"x");
        }
        t.feed(b"\x07");
        assert!(t.screen.title.is_empty());
    }

    #[test]
    fn dcs_is_swallowed() {
        let t = run(20, 2, "a\x1bP1$r0m\x1b\\b");
        assert_eq!(t.text(), "ab");
    }
}
