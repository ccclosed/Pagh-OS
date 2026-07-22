//! Typo suggestion: bounded edit distance and nearest-command lookup against
//! the registry (R7.2).
//!
//! Pure logic, `no_std`-friendly: distances are computed over Unicode
//! scalar values (UTF-8 safe) and the dynamic-programming row lives in a
//! heap `Vec` sized to the second string. Both functions are bounded by
//! `max` so large/very-different inputs stay cheap and never panic.
//!
//! Consumed by the shell dispatcher (later task) to offer "did you mean
//! '<name>'?" hints for unknown commands.

use alloc::vec::Vec;

/// Smallest of three values.
#[inline]
fn min3(a: usize, b: usize, c: usize) -> usize {
    let m = if a < b { a } else { b };
    if m < c {
        m
    } else {
        c
    }
}

/// Bounded Levenshtein edit distance between `a` and `b`, computed over
/// characters (Unicode scalar values, so multi-byte UTF-8 is handled
/// correctly).
///
/// Insertion, deletion, and substitution each cost 1. The result is the
/// exact distance when it is `<= max`; if the true distance would exceed
/// `max`, the function returns `max + 1` (it does not compute the precise
/// over-threshold value). This early-exit keeps both time and memory bounded
/// for large or very dissimilar inputs.
///
/// Never panics for any input.
#[allow(dead_code)]
pub fn edit_distance(a: &str, b: &str, max: usize) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();

    // The minimum possible distance is the difference in lengths (it takes at
    // least that many insertions/deletions). If that already exceeds `max`,
    // skip all allocation and DP work.
    let len_diff = if n > m { n - m } else { m - n };
    if len_diff > max {
        return max + 1;
    }

    // One-row dynamic programming over `b`. `prev[j]` holds the distance
    // between `a[..i]` and `b[..j]`. Row width is bounded by `b`'s char count.
    let mut prev: Vec<usize> = (0..=m).collect();

    for i in 1..=n {
        let mut cur: Vec<usize> = Vec::with_capacity(m + 1);
        // Distance between `a[..i]` and the empty prefix of `b` is `i`.
        cur.push(i);
        let mut row_min = i;

        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            let deletion = prev[j] + 1;
            let insertion = cur[j - 1] + 1;
            let substitution = prev[j - 1] + cost;
            let v = min3(deletion, insertion, substitution);
            cur.push(v);
            if v < row_min {
                row_min = v;
            }
        }

        // Early exit: any optimal alignment path must cross this row, so the
        // final distance is >= the minimum value in the row. Once that
        // minimum exceeds `max`, the true distance can only exceed `max`.
        if row_min > max {
            return max + 1;
        }

        prev = cur;
    }

    let d = prev[m];
    if d > max {
        max + 1
    } else {
        d
    }
}

/// Return the command name with the minimum bounded edit distance to `input`
/// that is within `max`, or `None` if no name is within `max`.
///
/// Ties are broken by iteration order: the first name achieving the minimum
/// distance wins. This is the inverse guarantee asserted by Property 26 (the
/// returned suggestion's distance is the minimum over all names).
#[allow(dead_code)]
pub fn nearest_command<'a>(input: &str, names: &[&'a str], max: usize) -> Option<&'a str> {
    let mut best: Option<(&'a str, usize)> = None;

    for &name in names {
        let d = edit_distance(input, name, max);
        if d <= max {
            match best {
                // Keep the earlier name on ties (strictly-closer replaces).
                Some((_, best_d)) if best_d <= d => {}
                _ => best = Some((name, d)),
            }
        }
    }

    best.map(|(name, _)| name)
}
