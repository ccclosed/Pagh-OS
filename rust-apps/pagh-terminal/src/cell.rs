//! Cells, colors, attributes and the pen.
//!
//! A cell stores colors symbolically (`Default`, palette index, or 24-bit RGB),
//! so a theme change repaints cells that were written with the default colors
//! without touching the grid.  Palette lookup and attribute resolution happen
//! in the renderer.

/// A 24-bit RGB color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);
    pub const WHITE: Rgb = Rgb(255, 255, 255);

    /// Parse `#rrggbb` (the leading `#` is optional). Returns `None` on any
    /// other shape, so untrusted OSC palette strings can be rejected cheaply.
    pub fn from_hex(text: &str) -> Option<Rgb> {
        let s = text.strip_prefix('#').unwrap_or(text);
        if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }

    /// Linear interpolation.
    pub fn lerp(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let f = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(f(self.0, other.0), f(self.1, other.1), f(self.2, other.2))
    }

    /// Scale brightness (used by the SGR dim attribute).
    pub fn scale(self, factor: f32) -> Rgb {
        let f = |v: u8| (v as f32 * factor).clamp(0.0, 255.0).round() as u8;
        Rgb(f(self.0), f(self.1), f(self.2))
    }

    pub fn to_array(self) -> [u8; 3] {
        [self.0, self.1, self.2]
    }
}

/// Symbolic foreground/background color of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    /// The theme's default foreground or background.
    #[default]
    Default,
    /// Index into the 256-color palette.
    Indexed(u8),
    /// Direct 24-bit color (SGR 38;2 / 48;2).
    Rgb(u8, u8, u8),
}

impl Color {
    /// Resolve against a palette and theme defaults.
    pub fn resolve(self, palette: &[Rgb; 256], default: Rgb) -> Rgb {
        match self {
            Color::Default => default,
            Color::Indexed(i) => palette[i as usize],
            Color::Rgb(r, g, b) => Rgb(r, g, b),
        }
    }

    pub fn is_default(self) -> bool {
        matches!(self, Color::Default)
    }
}

/// SGR attributes.  A plain newtype over a bitmask keeps `Cell` compact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs(pub u16);

impl Attrs {
    pub const BOLD: Attrs = Attrs(1 << 0);
    pub const DIM: Attrs = Attrs(1 << 1);
    pub const ITALIC: Attrs = Attrs(1 << 2);
    pub const UNDERLINE: Attrs = Attrs(1 << 3);
    pub const BLINK: Attrs = Attrs(1 << 4);
    pub const REVERSE: Attrs = Attrs(1 << 5);
    pub const HIDDEN: Attrs = Attrs(1 << 6);
    pub const STRIKETHROUGH: Attrs = Attrs(1 << 7);
    pub const OVERLINE: Attrs = Attrs(1 << 8);

    pub const fn empty() -> Attrs {
        Attrs(0)
    }

    pub const fn contains(self, other: Attrs) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Attrs) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Attrs) {
        self.0 &= !other.0;
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn bits(self) -> u16 {
        self.0
    }
}

/// Cell flags used for wide-character bookkeeping.
pub const CELL_WIDE: u8 = 1 << 0;
/// This cell is the right half of a wide character; it has no glyph of its own.
pub const CELL_WIDE_CONT: u8 = 1 << 1;

/// One terminal cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
            flags: 0,
        }
    }
}

impl Cell {
    /// A blank cell occupying the given pen background (BCE semantics: erase
    /// operations paint the current background color).
    pub fn blank(bg: Color) -> Cell {
        Cell {
            ch: ' ',
            bg,
            ..Cell::default()
        }
    }

    /// True when the cell contains no visible glyph and no attributes that
    /// paint pixels.  Used by text extraction to trim trailing blanks.
    pub fn is_blank(&self) -> bool {
        (self.ch == ' ' || self.ch == '\0')
            && self.flags & CELL_WIDE == 0
            && self.bg.is_default()
            && self.attrs.is_empty()
    }

    pub fn is_wide(&self) -> bool {
        self.flags & CELL_WIDE != 0
    }

    pub fn is_wide_continuation(&self) -> bool {
        self.flags & CELL_WIDE_CONT != 0
    }
}

/// The current SGR pen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Default for Pen {
    fn default() -> Self {
        Pen {
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
        }
    }
}

impl Pen {
    pub fn cell(self, ch: char, flags: u8) -> Cell {
        Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
            attrs: self.attrs,
            flags,
        }
    }
}

/// The standard xterm 256-color palette, tuned slightly toward Pagh's teal
/// surfaces.  Indices 0-15 are the ANSI colors, 16-231 the 6x6x6 cube and
/// 232-255 the grayscale ramp.
pub fn default_palette() -> [Rgb; 256] {
    // Stock Breeze-dark ANSI 16.  These are the long-published color values of
    // the desktop's terminal scheme, used as a visual reference only; the
    // block below is original Pagh code.  Indices 16-255 are the standard
    // xterm cube and grayscale ramp, independent of any theme.
    let ansi = [
        Rgb(0x23, 0x26, 0x27), // black
        Rgb(0xed, 0x15, 0x15), // red
        Rgb(0x11, 0xd1, 0x16), // green
        Rgb(0xf6, 0x74, 0x00), // yellow
        Rgb(0x1d, 0x99, 0xf3), // blue
        Rgb(0x9b, 0x59, 0xb6), // magenta
        Rgb(0x1a, 0xbc, 0x9c), // cyan
        Rgb(0xfc, 0xfc, 0xfc), // white
        Rgb(0x7f, 0x8c, 0x8d), // bright black
        Rgb(0xc0, 0x39, 0x2b), // bright red
        Rgb(0x1c, 0xdc, 0x9a), // bright green
        Rgb(0xfd, 0xbc, 0x4b), // bright yellow
        Rgb(0x3d, 0xae, 0xe9), // bright blue
        Rgb(0x8e, 0x44, 0xad), // bright magenta
        Rgb(0x16, 0xa0, 0x85), // bright cyan
        Rgb(0xff, 0xff, 0xff), // bright white
    ];
    let mut palette = [Rgb::BLACK; 256];
    palette[..16].copy_from_slice(&ansi);
    let levels = [0u8, 95, 135, 175, 215, 255];
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                palette[16 + r * 36 + g * 6 + b] = Rgb(levels[r], levels[g], levels[b]);
            }
        }
    }
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        palette[232 + i] = Rgb(v, v, v);
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_cube_and_gray() {
        let p = default_palette();
        assert_eq!(p[16], Rgb(0, 0, 0));
        assert_eq!(p[21], Rgb(0, 0, 255));
        assert_eq!(p[196], Rgb(255, 0, 0));
        assert_eq!(p[231], Rgb(255, 255, 255));
        assert_eq!(p[232], Rgb(8, 8, 8));
        assert_eq!(p[255], Rgb(238, 238, 238));
    }

    #[test]
    fn resolve_colors() {
        let p = default_palette();
        assert_eq!(Color::Default.resolve(&p, Rgb(1, 2, 3)), Rgb(1, 2, 3));
        assert_eq!(Color::Indexed(1).resolve(&p, Rgb::BLACK), p[1]);
        assert_eq!(Color::Rgb(9, 8, 7).resolve(&p, Rgb::BLACK), Rgb(9, 8, 7));
    }

    #[test]
    fn attrs_are_a_bitmask() {
        let mut a = Attrs::BOLD;
        a.insert(Attrs::UNDERLINE);
        assert!(a.contains(Attrs::BOLD));
        assert!(a.contains(Attrs::UNDERLINE));
        a.remove(Attrs::BOLD);
        assert!(!a.contains(Attrs::BOLD));
    }

    #[test]
    fn rgb_hex() {
        assert_eq!(Rgb::from_hex("#2fd4b0"), Some(Rgb(0x2f, 0xd4, 0xb0)));
        assert_eq!(Rgb::from_hex("2fd4b0"), Some(Rgb(0x2f, 0xd4, 0xb0)));
        assert_eq!(Rgb::from_hex("#2fd4b"), None);
        assert_eq!(Rgb::from_hex("#zzzzzz"), None);
    }
}
