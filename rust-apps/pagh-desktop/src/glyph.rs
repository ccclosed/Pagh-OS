//! Кэш растровых масок глифов («glyph atlas»).
//!
//! Шрифт композитора — фиксированный 8×16, поэтому маска глифа зависит только от
//! символа и масштаба. Кэш хранит для каждой маски горизонтальные runs закрашенных
//! пикселей: рисование текста больше не перебирает 16×8 бит и не проверяет их на
//! каждом кадре. Цвет и прозрачность в ключ не входят — маска одна на все цвета.

use crate::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH};
use std::collections::HashMap;
use std::sync::Arc;

/// Максимум масок в кэше; при переполнении кэш очищается (редкий случай).
const MAX_MASKS: usize = 1024;

/// Строка маски: список `(x, width)` закрашенных runs.
pub type GlyphRuns = Vec<(i32, i32)>;

/// Закэшированная маска глифа в пикселях холста.
#[derive(Clone, Debug)]
pub struct GlyphMask {
    pub width: i32,
    pub height: i32,
    /// `rows[y - y0]` — runs для строки; `y0` всегда 0, маска обрезана по глифу.
    rows: Vec<GlyphRuns>,
}

impl GlyphMask {
    pub fn rows(&self) -> &[GlyphRuns] {
        &self.rows
    }
}

/// Кэш масок (char, scale).
#[derive(Default)]
pub struct GlyphAtlas {
    masks: HashMap<(char, i32), Arc<GlyphMask>>,
    hits: u64,
    misses: u64,
}

impl GlyphAtlas {
    /// Маска глифа; `None` — символа нет даже в fallback `?`.
    pub fn mask(&mut self, ch: char, scale: i32) -> Option<Arc<GlyphMask>> {
        let scale = scale.max(1);
        let key = (ch, scale);
        if let Some(mask) = self.masks.get(&key) {
            self.hits += 1;
            return Some(mask.clone());
        }
        let bitmap = font::glyph(ch).or_else(|| font::glyph('?'))?;
        self.misses += 1;
        if self.masks.len() >= MAX_MASKS {
            self.masks.clear();
        }
        let mask = Arc::new(build_mask(&bitmap, scale));
        self.masks.insert(key, mask.clone());
        Some(mask)
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn len(&self) -> usize {
        self.masks.len()
    }
}

fn build_mask(bitmap: &[u8; 16], scale: i32) -> GlyphMask {
    let scale = scale.max(1);
    let mut rows: Vec<GlyphRuns> = Vec::with_capacity((GLYPH_HEIGHT * scale) as usize);
    for bits in bitmap.iter() {
        // Собираем runs по исходным 8 пикселям, растягивая каждый на `scale`.
        let mut runs = Vec::new();
        let mut x = 0;
        while x < GLYPH_WIDTH {
            if bits & (0x80 >> x) != 0 {
                let start = x;
                while x < GLYPH_WIDTH && bits & (0x80 >> x) != 0 {
                    x += 1;
                }
                runs.push((start * scale, (x - start) as i32 * scale));
            } else {
                x += 1;
            }
        }
        for _ in 0..scale {
            rows.push(runs.clone());
        }
    }
    GlyphMask {
        width: GLYPH_WIDTH * scale,
        height: GLYPH_HEIGHT * scale,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_is_cached_and_has_runs() {
        let mut atlas = GlyphAtlas::default();
        let first = atlas.mask('A', 1).expect("глиф A");
        assert!(!first.rows().iter().all(|row| row.is_empty()));
        let second = atlas.mask('A', 1).expect("глиф A повторно");
        assert_eq!(atlas.misses(), 1);
        assert_eq!(atlas.hits(), 1);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn scale_doubles_mask_extent() {
        let mut atlas = GlyphAtlas::default();
        let small = atlas.mask('A', 1).unwrap();
        let large = atlas.mask('A', 2).unwrap();
        assert_eq!(small.width, 8);
        assert_eq!(small.height, 16);
        assert_eq!(large.width, 16);
        assert_eq!(large.height, 32);
        let runs: i32 = large
            .rows()
            .iter()
            .flat_map(|row| row.iter())
            .map(|(_, w)| w)
            .sum();
        let small_runs: i32 = small
            .rows()
            .iter()
            .flat_map(|row| row.iter())
            .map(|(_, w)| w)
            .sum();
        assert_eq!(runs, small_runs * 4);
    }
}
