//! Tab-дополнение: имена команд и путей плюс логика longest-common-prefix (R3).
//!
//! Всё здесь — **чистая логика над данными**: без консоли, клавиатуры и VFS.
//! Записи каталога передаёт вызывающий (интерактивный цикл читает их из VFS и
//! передаёт как `&[&str]`), поэтому сопоставление и вычисление
//! longest-common-prefix детерминированно проверяются in-kernel property-тестами
//! (Property 24).
//!
//! Эти элементы потребляет интерактивный цикл; до подключения они не
//! упоминаются внутри, отсюда `#[allow(dead_code)]`, чтобы сборка `#![no_std]`
//! оставалась без предупреждений.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Посимвольный longest common prefix `candidates`.
///
/// Результат гарантированно (a) префикс *каждого* кандидата и (b) **максимальный** —
/// нет более длинной строки, которая тоже префикс каждого кандидата. Сравнение
/// идёт по Unicode scalar values (`char`), никогда по сырым байтам, поэтому
/// возвращённый `String` всегда валидный UTF-8 и никогда не разрезает
/// многобайтовый символ (R11.6).
///
/// Крайние случаи:
/// - пустой срез возвращает `""` (общего содержимого нет),
/// - единственный кандидат возвращается без изменений.
#[allow(dead_code)]
pub fn longest_common_prefix(candidates: &[&str]) -> String {
    // Нет кандидатов: общего ничего.
    let (first, rest) = match candidates.split_first() {
        Some(parts) => parts,
        None => return String::new(),
    };

    // Идти по символам первого кандидата; префикс не может быть длиннее
    // самого короткого кандидата, и первый кандидат его ограничивает.
    let mut prefix = String::new();
    for (idx, ch) in first.char_indices() {
        // Каждый другой кандидат должен иметь ровно этот символ на этой позиции.
        // Сравниваем по символам, двигая итератор каждого кандидата синхронно с
        // `idx` через поиск `char_indices`.
        let all_match = rest.iter().all(|cand| {
            // Символ `cand`, начинающийся с того же char-индекса, что и `ch`.
            cand[idx..].chars().next() == Some(ch)
        });

        if all_match {
            prefix.push(ch);
        } else {
            break;
        }
    }

    prefix
}

/// Результат запроса дополнения (R3.3/R3.4/R3.5).
///
/// Контракт для вызывающего (интерактивный цикл):
/// - [`Completion::None`] — ни один кандидат не совпал; оставить строку без изменений
/// - [`Completion::Single`] — совпал ровно один кандидат; обёрнутая строка —
///   **полный дополненный токен/сегмент**, так что приложение приравнивает
///   токен к этой строке (R3.3, Property 24).
/// - [`Completion::Multiple`] — совпало несколько кандидатов; `lcp` — longest
///   common prefix, до которого расширить токен, а `candidates` — список для
///   показа. Каждая запись `candidates` (и сам `lcp`) начинается с
///   изначально набранного префикса/сегмента (R3.4).
#[allow(dead_code)]
pub enum Completion {
    /// Нет совпадения — вызывающий оставляет строку без изменений (R3.5).
    None,
    /// Единственное совпадение — обёрнутое значение — полный дополненный токен (R3.3).
    Single(String),
    /// Несколько совпадений — расширить до `lcp`, затем показать `candidates` (R3.4).
    Multiple {
        /// Longest common prefix всех совпадений (>= набранного префикса).
        lcp: String,
        /// Совпавшие строки-кандидаты для показа пользователю.
        candidates: Vec<String>,
    },
}

/// Построить [`Completion`] из набора совпавших строк.
///
/// Общий для [`complete_command`] и [`complete_path`]: 0 совпадений → `None`,
/// 1 → `Single`, `>= 2` → `Multiple { lcp, candidates }`.
fn from_matches(matches: Vec<&str>) -> Completion {
    match matches.len() {
        0 => Completion::None,
        1 => Completion::Single(matches[0].to_string()),
        _ => {
            let lcp = longest_common_prefix(&matches);
            let candidates = matches.iter().map(|s| s.to_string()).collect();
            Completion::Multiple { lcp, candidates }
        }
    }
}

/// Дополнить имя команды по реестру (R3.1).
///
/// Собирает каждое зарегистрированное имя команды (через
/// [`crate::shell::registry::command_names`]), начинающееся с `prefix`, затем:
/// 0 совпадений → [`Completion::None`] (R3.5), 1 → [`Completion::Single`] (R3.3),
/// `>= 2` → [`Completion::Multiple`] с longest common prefix и списком
/// кандидатов (R3.4). Каждый возвращённый кандидат начинается с `prefix`.
#[allow(dead_code)]
pub fn complete_command(prefix: &str) -> Completion {
    let matches: Vec<&str> = crate::shell::registry::command_names()
        .filter(|name| name.starts_with(prefix))
        .collect();
    from_matches(matches)
}

/// Дополнить сегмент файлового пути по переданным записям каталога (R3.2).
///
/// Это **чисто**: `dir_entries` — имена в соответствующем каталоге, переданные
/// вызывающим как данные (VFS здесь не вызывается). Сопоставление идёт по
/// **последнему сегменту** `partial` — тексту после финального `/`, — поэтому
/// вызывающий отвечает за вставку дополненного сегмента обратно в полный токен.
/// Возвращаемые [`Completion::Single`] / [`Completion::Multiple`] несут поэтому
/// совпавшее **имя записи**, каждое из которых начинается с этого последнего
/// сегмента (Property 24).
///
/// `cwd` сохранён для совместимости сигнатуры; логика префикса здесь зависит
/// только от последнего сегмента `partial` и переданных записей.
#[allow(dead_code)]
pub fn complete_path(cwd: &str, partial: &str, dir_entries: &[&str]) -> Completion {
    let _ = cwd; // retained per design signature; matching uses `partial`.

    // Дополняемый сегмент — текст после последнего '/'. Если '/' нет,
    // весь `partial` и есть сегмент.
    let segment = match partial.rfind('/') {
        Some(idx) => &partial[idx + 1..],
        None => partial,
    };

    let matches: Vec<&str> = dir_entries
        .iter()
        .copied()
        .filter(|entry| entry.starts_with(segment))
        .collect();
    from_matches(matches)
}
