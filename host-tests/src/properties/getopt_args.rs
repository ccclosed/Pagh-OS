//! Host-свойства чистого getopt-парсера (`src/shell/getopt.rs`).
//!
//! Свойства описывают правила POSIX/GNU, а не порядок операторов в реализации:
//!
//! * кластер `-lab` эквивалентен последовательности `-l -a -b` (и повторы —
//!   тоже повторы в результате);
//! * `--` — жёсткая граница: всё после него операнды дословно, включая
//!   `-l`/`--`;
//! * неизвестная короткая/длинная опция — ровно `Err(UnknownShort/Long)` (не
//!   «пропустить и продолжить»), причём называется первый проблемный токен/буква;
//! * аргумент опции равносилен приклеенному и отдельному написанию (`-n5` =
//!   `-n 5`), включая значение, начинающееся с `-`;
//! * перестановка опций среди операндов не меняет ни порядок операндов, ни набор
//!   флагов (GNU permutation);
//! * произвольный вход не паникует — только `Ok` или `Err`.

use crate::getopt::{long, parse, short, short_long, short_value, Error, Item, OptSpec};
use proptest::prelude::*;

/// Спецификация, над которой гоняются все свойства: три флага, один флаг с
/// длинным написанием и одна опция с обязательным аргументом.
const SPECS: &[OptSpec] = &[
    short('l'),
    short('a'),
    short('b'),
    short_long('h', "human"),
    short_value('n'),
];

/// Пустая таблица: любые `-x`/`--x` — ошибка, операнды проходят дословно.
const NO_OPTIONS: &[OptSpec] = &[];

/// Одна опция-флаг — второй путь разбора в property ниже.
const ONE_FLAG: &[OptSpec] = &[short('x')];

/// Длинный флаг и опция с аргументом — третий путь разбора.
const LONG_AND_VALUE: &[OptSpec] = &[long("only"), short_value('v')];

/// Флаги (в порядке появления) из разобранного результата.
fn flags(items: &[Item<'_>]) -> Vec<char> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Opt {
                spec: OptSpec { short: Some(ch), .. },
                ..
            } => Some(*ch),
            _ => None,
        })
        .collect()
}

/// Операнды (в порядке появления).
fn operands<'a>(items: &[Item<'a>]) -> Vec<&'a str> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Operand(text) => Some(*text),
            _ => None,
        })
        .collect()
}

/// Независимый оракул для кластера: каждая буква — отдельная опция.
fn expected_cluster_flags(cluster: &str) -> Result<Vec<char>, char> {
    let mut out = Vec::new();
    for ch in cluster.chars() {
        // 'h' — флаг с длинным написанием; 'n' требует аргумент и в кластере
        // без хвоста не разбирается (свойства ниже берут только флаги).
        if ch == 'l' || ch == 'a' || ch == 'b' || ch == 'h' {
            out.push(ch);
        } else {
            return Err(ch);
        }
    }
    Ok(out)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Группировка коротких флагов — ровно их последовательность.
    #[test]
    fn grouped_flags_equal_separate_flags(
        cluster in "[labh]{1,6}",
        operand in "[A-Za-z0-9_./]{0,10}"
    ) {
        let token = std::format!("-{}", cluster);
        let cluster_items = parse(&[token.as_str()], SPECS).expect("known cluster");
        let expected = expected_cluster_flags(&cluster).expect("generated from known chars");
        prop_assert_eq!(flags(&cluster_items), expected);

        let separate: Vec<std::string::String> =
            cluster.chars().map(|ch| std::format!("-{}", ch)).collect();
        let separate_refs: Vec<&str> = separate.iter().map(|s| s.as_str()).collect();
        let mut args = separate_refs.clone();
        if !operand.is_empty() {
            args.push(operand.as_str());
        }
        let separate_items = parse(&args, SPECS).expect("known flags");
        prop_assert_eq!(flags(&separate_items), flags(&cluster_items));
    }

    /// `--` завершает опции: последующие токены — операнды дословно, и сам `--`
    /// в результат не попадает.
    #[test]
    fn double_dash_makes_everything_after_it_an_operand(
        before in "[la]{0,3}",
        after in prop::collection::vec("[-A-Za-z0-9_.=/]{0,8}", 0..4)
    ) {
        let mut tokens: Vec<&str> = Vec::new();
        let flag_token;
        if !before.is_empty() {
            flag_token = std::format!("-{}", before);
            tokens.push(flag_token.as_str());
        }
        tokens.push("--");
        for token in &after {
            tokens.push(token.as_str());
        }
        let items = parse(&tokens, SPECS).expect("verbs are known");
        prop_assert_eq!(flags(&items), expected_cluster_flags(&before).expect("known chars"));
        prop_assert_eq!(operands(&items), after.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        prop_assert!(!operands(&items).contains(&"--"));
    }

    /// Неизвестная буква в кластере — ошибка на первой же неизвестной, а не
    /// молчаливый пропуск.
    #[test]
    fn unknown_short_is_reported_at_the_first_unknown_char(cluster in "[labhxyz]{1,6}") {
        let token = std::format!("-{}", cluster);
        match expected_cluster_flags(&cluster) {
            Ok(expected) => {
                let items = parse(&[token.as_str()], SPECS).expect("all chars known");
                prop_assert_eq!(flags(&items), expected);
            }
            Err(bad) => {
                prop_assert_eq!(parse(&[token.as_str()], SPECS), Err(Error::UnknownShort(bad)));
            }
        }
    }

    /// Длинные опции: известное имя принимается, неизвестное — `UnknownLong`,
    /// а `=значение` у флага — `UnexpectedValue` (не тихий no-op).
    #[test]
    fn long_options_follow_their_declaration(
        name in "[a-z]{1,8}",
        value in prop::option::of("[A-Za-z0-9]{0,6}")
    ) {
        let token = match &value {
            Some(value) => std::format!("--{}={}", name, value),
            None => std::format!("--{}", name),
        };
        let parsed = parse(&[token.as_str()], SPECS);
        if name == "human" {
            // Объявленный флаг: `--human` проходит, `--human=v` — ошибка.
            if value.is_some() {
                prop_assert_eq!(parsed, Err(Error::UnexpectedValue(&SPECS[3])));
            } else {
                let items = parsed.expect("declared long flag");
                prop_assert_eq!(flags(&items), vec!['h']);
                prop_assert!(operands(&items).is_empty());
            }
        } else {
            // Неизвестное имя — неизвестно и с `=значением`.
            prop_assert_eq!(parsed, Err(Error::UnknownLong(name.as_str())));
        }
    }

    /// Аргумент опции: `-n5` и `-n 5` неразличимы; значение может начинаться с
    /// `-` (getopt забирает следующий токен даже таким).
    #[test]
    fn option_value_attached_or_detached_are_equivalent(
        value in "-?[A-Za-z0-9_-]{1,8}"
    ) {
        let attached = std::format!("-n{}", value);
        let attached_items = parse(&[attached.as_str()], SPECS).expect("value attached");
        let separate_items = parse(&["-n", value.as_str()], SPECS).expect("value detached");
        prop_assert_eq!(&attached_items, &separate_items);
        // Значение — ровно сгенерированная строка, а не её часть.
        match attached_items.as_slice() {
            [Item::Opt { value: Some(got), .. }] => prop_assert_eq!(*got, value.as_str()),
            other => prop_assert!(false, "expected one option with value, got {:?}", other),
        }
    }

    /// Опция с аргументом в конце строки — ошибка `MissingValue`, а не паника и
    /// не пустое значение.
    #[test]
    fn missing_value_is_an_error(_seed in any::<u8>()) {
        prop_assert_eq!(parse(&["-n"], SPECS), Err(Error::MissingValue(&SPECS[4])));
        prop_assert_eq!(
            parse(&["-ln"], SPECS),
            Err(Error::MissingValue(&SPECS[4]))
        );
    }

    /// Перестановка опций среди операндов не меняет порядок операндов и набор
    /// флагов (GNU permutation).
    #[test]
    fn permuting_options_preserves_operands(
        operand_count in 1usize..5,
        flags_before in "[lab]{0,4}",
        flags_after in "[lab]{0,4}",
    ) {
        let operands_gen: Vec<std::string::String> = (0..operand_count)
            .map(|i| std::format!("/mnt/op{}", i))
            .collect();
        let before_token = if flags_before.is_empty() {
            std::string::String::new()
        } else {
            std::format!("-{}", flags_before)
        };
        let after_token = if flags_after.is_empty() {
            std::string::String::new()
        } else {
            std::format!("-{}", flags_after)
        };

        // Все опции впереди.
        let mut front: Vec<&str> = Vec::new();
        if !before_token.is_empty() {
            front.push(before_token.as_str());
        }
        if !after_token.is_empty() {
            front.push(after_token.as_str());
        }
        front.extend(operands_gen.iter().map(|s| s.as_str()));
        // Опции вперемешку: по одной между операндами.
        let mut mixed: Vec<&str> = Vec::new();
        if !before_token.is_empty() {
            mixed.push(before_token.as_str());
        }
        for (i, operand) in operands_gen.iter().enumerate() {
            mixed.push(operand.as_str());
            if i == 0 && !after_token.is_empty() {
                mixed.push(after_token.as_str());
            }
        }

        let front_items = parse(&front, SPECS).expect("known flags");
        let mixed_items = parse(&mixed, SPECS).expect("known flags");
        let mut front_flags = flags(&front_items);
        let mut mixed_flags = flags(&mixed_items);
        front_flags.sort_unstable();
        mixed_flags.sort_unstable();
        prop_assert_eq!(front_flags, mixed_flags);
        prop_assert_eq!(
            operands(&front_items),
            operands_gen.iter().map(|s| s.as_str()).collect::<Vec<_>>()
        );
        prop_assert_eq!(
            operands(&mixed_items),
            operands_gen.iter().map(|s| s.as_str()).collect::<Vec<_>>()
        );
    }

    /// Один `-` — операнд, а не пустая группа опций.
    #[test]
    fn single_dash_is_an_operand(_seed in any::<u8>()) {
        let items = parse(&["-"], SPECS).expect("single dash");
        prop_assert_eq!(operands(&items), vec!["-"]);
    }

    /// Любой вход — либо `Ok`, либо `Err`, но никогда паника (парсер режет
    /// строки по байтовым смещениям; свойство ловит ошибку на не-ASCII).
    #[test]
    fn arbitrary_tokens_never_panic(tokens in prop::collection::vec(any::<String>(), 0..5)) {
        let refs: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
        let _ = parse(&refs, SPECS);
        let _ = parse(&refs, NO_OPTIONS);
        // Спецификация с одной опцией без аргумента и с длинной/значением —
        // другие пути разбора (короткий кластер против `--name=value`).
        let _ = parse(&refs, ONE_FLAG);
        let _ = parse(&refs, LONG_AND_VALUE);
    }

}