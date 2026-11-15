//! Подсказка опечаток: ограниченное edit distance и поиск ближайшей команды по
//! реестру (R7.2).
//!
//! Чистая логика, `no_std`-friendly: расстояния считаются по Unicode scalar
//! values (UTF-8-safe), а строка dynamic programming живёт в heap-`Vec` размером
//! со вторую строку. Обе функции ограничены `max`, так что большие/очень разные
//! входы остаются дешёвыми и никогда не паникуют.
//!
//! Потребляется диспетчером shell, чтобы предлагать подсказки «did you mean
//! '<name>'?» для неизвестных команд.

use alloc::vec::Vec;

/// Наименьшее из трёх значений.
#[inline]
fn min3(a: usize, b: usize, c: usize) -> usize {
    let m = if a < b { a } else { b };
    if m < c {
        m
    } else {
        c
    }
}

/// Ограниченное Levenshtein-расстояние между `a` и `b`, посчитанное по
/// символам (Unicode scalar values, так что многобайтовый UTF-8 обрабатывается
/// корректно).
///
/// Вставка, удаление и замена стоят по 1. Результат — точное расстояние, когда
/// оно `<= max`; если истинное расстояние превышает `max`, функция возвращает
/// `max + 1` (точное значение за порогом не вычисляется). Этот early-exit
/// держит время и память ограниченными для больших или очень разных входов.
///
/// Никогда не паникует ни на каком входе.
#[allow(dead_code)]
pub fn edit_distance(a: &str, b: &str, max: usize) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();

    // Минимально возможное расстояние — разность длин (нужно как минимум
    // столько вставок/удалений). Если она уже превышает `max`, пропустить всю
    // аллокацию и DP.
    let len_diff = if n > m { n - m } else { m - n };
    if len_diff > max {
        return max + 1;
    }

    // Однострочное dynamic programming по `b`. `prev[j]` хранит расстояние между
    // `a[..i]` и `b[..j]`. Ширина строки ограничена числом символов `b`.
    let mut prev: Vec<usize> = (0..=m).collect();

    for i in 1..=n {
        let mut cur: Vec<usize> = Vec::with_capacity(m + 1);
        // Расстояние между `a[..i]` и пустым префиксом `b` равно `i`.
        cur.push(i);
        let mut row_min = i;

        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            let deletion = prev[j] + 1;
            let insertion = cur[j - 1] + 1;
            let substitution = prev[j - 1] + cost;
            let v = min3(deletion, insertion, substitution);
            cur.push(v);
            if v < row_min {
                row_min = v;
            }
        }

        // Early exit: любой оптимальный путь выравнивания пересекает эту строку,
        // так что итоговое расстояние >= минимума в строке. Как только минимум
        // превышает `max`, истинное расстояние может только превышать `max`.
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

/// Вернуть имя команды с минимальным ограниченным edit distance до `input` в
/// пределах `max` или `None`, если ни одно имя не укладывается в `max`.
///
/// Ничьи разрешаются порядком итерации: побеждает первое имя с минимальным
/// расстоянием. Это обратная гарантия свойства 26 (расстояние возвращённой
/// подсказки минимально среди всех имён).
#[allow(dead_code)]
pub fn nearest_command<'a>(input: &str, names: &[&'a str], max: usize) -> Option<&'a str> {
    let mut best: Option<(&'a str, usize)> = None;

    for &name in names {
        let d = edit_distance(input, name, max);
        if d <= max {
            match best {
                // При ничьей оставить более раннее имя (строго ближе — заменяет).
                Some((_, best_d)) if best_d <= d => {}
                _ => best = Some((name, d)),
            }
        }
    }

    best.map(|(name, _)| name)
}
