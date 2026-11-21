//! Чистый getopt-парсер в стиле POSIX/GNU для shell-команд.
//!
//! Команды объявляют свои опции таблицей [`OptSpec`] и получают разобранные
//! элементы ([`Item`]) — без ручных `if a == "-l"`, каждый со своей копией
//! правил. Парсер — `core` + `alloc`-only, без VFS/консоли, поэтому host-свойство
//! `getopt_args` исполняет ровно тот исходник, который компилирует ядро.
//!
//! # Правила
//!
//! * Короткие опции группируются: `-la` = `-l -a`.
//! * У короткой опции с аргументом ([`short_value`]) аргумент может быть
//!   приклеен (`-n5`) или идти следующим токеном (`-n 5`), как в getopt(3).
//! * Длинные опции: `--human` и `--progress=ansi`. Длинная опция без аргумента
//!   с `=значением` — ошибка [`Error::UnexpectedValue`], а не молчаливое
//!   игнорирование.
//! * `--` завершает разбор опций: все следующие токены — операнды, даже если
//!   начинаются с `-`. Сам `--` в результат не попадает.
//! * Один `-` — тоже операнд (POSIX: это имя файла, а не опция).
//! * Опции могут стоять вперемешку с операндами (GNU permutation): `ls /mnt -l`
//!   разбирается так же, как `ls -l /mnt`. Операнды сохраняют порядок.
//! * Неизвестная опция — ошибка ([`Error::UnknownShort`] /
//!   [`Error::UnknownLong`]), которую вызывающий печатает вместе с usage.
//!   Молчаливого «съесть и продолжить» нет: опечатка в скрипте не должна
//!   выглядеть как успешный запуск.
//!
//! Разбор никогда не паникует: любой вход — это либо `Ok(items)`, либо
//! `Err(error)`.

use alloc::vec::Vec;

/// Нужен ли опции аргумент.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptArg {
    /// Флаг: аргумента нет.
    None,
    /// Опция требует аргумент (следующий токен или `=value`/приклеенный хвост).
    Required,
}

/// Описание одной опции команды.
///
/// `short` и `long` могут быть заданы оба — тогда это одна и та же опция в двух
/// написаниях (`-h`/`--human`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptSpec {
    /// Короткая буква (`-l`), если есть.
    pub short: Option<char>,
    /// Длинное имя без `--` (`--human`), если есть.
    pub long: Option<&'static str>,
    /// Требуется ли аргумент.
    pub arg: OptArg,
}

/// Опция-флаг с коротким написанием.
pub const fn short(ch: char) -> OptSpec {
    OptSpec {
        short: Some(ch),
        long: None,
        arg: OptArg::None,
    }
}

/// Опция-флаг только с длинным написанием.
pub const fn long(name: &'static str) -> OptSpec {
    OptSpec {
        short: None,
        long: Some(name),
        arg: OptArg::None,
    }
}

/// Опция-флаг с коротким и длинным написанием.
///
/// Конструкторы ниже ядро пока не использует (его командам хватает `short`,
/// `long` и `short_value`), но host-свойство `getopt_args` и следующая волна
/// CLI-форм строятся на них; держать таблицу опций выразимой дешевле, чем
/// переписывать её при первом же `-h/--help`.
#[allow(dead_code)]
pub const fn short_long(ch: char, name: &'static str) -> OptSpec {
    OptSpec {
        short: Some(ch),
        long: Some(name),
        arg: OptArg::None,
    }
}

/// Опция с аргументом, короткое написание.
#[allow(dead_code)]
pub const fn short_value(ch: char) -> OptSpec {
    OptSpec {
        short: Some(ch),
        long: None,
        arg: OptArg::Required,
    }
}

/// Опция с аргументом, длинное написание.
#[allow(dead_code)]
pub const fn long_value(name: &'static str) -> OptSpec {
    OptSpec {
        short: None,
        long: Some(name),
        arg: OptArg::Required,
    }
}

/// Опция с аргументом, короткое и длинное написание.
#[allow(dead_code)]
pub const fn short_long_value(ch: char, name: &'static str) -> OptSpec {
    OptSpec {
        short: Some(ch),
        long: Some(name),
        arg: OptArg::Required,
    }
}

/// Результат разбора: найденная опция (со значением, если оно есть) или
/// операнд в том порядке, в каком они шли в строке.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    /// Опция; `value` — `Some` только для [`OptArg::Required`].
    Opt {
        /// Описание из таблицы команды.
        spec: &'static OptSpec,
        /// Аргумент опции, если он требуется.
        value: Option<&'a str>,
    },
    /// Не-опция: путь, шаблон, имя команды и т.п.
    Operand(&'a str),
}

/// Почему разбор не удался.
///
/// `Display` даёт готовую строку для `error_line`; usage дописывает вызывающий,
/// потому что только он знает свой синтаксис.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<'a> {
    /// Символ в короткой группе не объявлен командой.
    UnknownShort(char),
    /// Длинное имя не объявлено командой (имя без `--`).
    UnknownLong(&'a str),
    /// У опции кончился обязательный аргумент.
    MissingValue(&'static OptSpec),
    /// Опция без аргумента получила `=значение`.
    UnexpectedValue(&'static OptSpec),
}

impl core::fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::UnknownShort(ch) => write!(f, "unknown option: -{}", ch),
            Error::UnknownLong(name) => write!(f, "unknown option: --{}", name),
            Error::MissingValue(spec) => match (spec.short, spec.long) {
                (Some(ch), _) => write!(f, "option requires an argument: -{}", ch),
                (None, Some(name)) => write!(f, "option requires an argument: --{}", name),
                (None, None) => write!(f, "option requires an argument"),
            },
            Error::UnexpectedValue(spec) => match (spec.short, spec.long) {
                (Some(ch), _) => write!(f, "option does not take a value: -{}", ch),
                (None, Some(name)) => write!(f, "option does not take a value: --{}", name),
                (None, None) => write!(f, "option does not take a value"),
            },
        }
    }
}

/// Разобрать `args` по таблице `specs`.
///
/// Ошибка возвращается на первой же неизвестной опции/нехватке аргумента —
/// вызывающий печатает её с usage и ничего не исполняет.
pub fn parse<'a>(args: &[&'a str], specs: &'static [OptSpec]) -> Result<Vec<Item<'a>>, Error<'a>> {
    let mut out = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let token = args[index];
        index += 1;

        if token == "--" {
            // Все оставшиеся токены — операнды, включая начинающиеся с '-'.
            // Сам `--` в результат не попадает (getopt(3)).
            while index < args.len() {
                out.push(Item::Operand(args[index]));
                index += 1;
            }
            break;
        }

        if let Some(rest) = token.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (rest, None),
            };
            let Some(spec) = specs.iter().find(|spec| spec.long == Some(name)) else {
                return Err(Error::UnknownLong(name));
            };
            let value = match spec.arg {
                OptArg::None => {
                    if inline.is_some() {
                        return Err(Error::UnexpectedValue(spec));
                    }
                    None
                }
                OptArg::Required => match inline {
                    Some(value) => Some(value),
                    None => {
                        // Как getopt_long: аргумент можно взять следующим токеном.
                        let Some(value) = args.get(index).copied() else {
                            return Err(Error::MissingValue(spec));
                        };
                        index += 1;
                        Some(value)
                    }
                },
            };
            out.push(Item::Opt { spec, value });
            continue;
        }

        // Один `-` — операнд, а не пустая группа опций.
        if token.len() > 1 && token.starts_with('-') {
            let body = &token[1..];
            let mut chars = body.char_indices();
            while let Some((offset, ch)) = chars.next() {
                let Some(spec) = specs.iter().find(|spec| spec.short == Some(ch)) else {
                    return Err(Error::UnknownShort(ch));
                };
                if spec.arg == OptArg::Required {
                    // Хвост группы (если есть) — аргумент; иначе следующий токен.
                    let rest = &body[offset + ch.len_utf8()..];
                    let value = if !rest.is_empty() {
                        rest
                    } else {
                        let Some(value) = args.get(index).copied() else {
                            return Err(Error::MissingValue(spec));
                        };
                        index += 1;
                        value
                    };
                    out.push(Item::Opt {
                        spec,
                        value: Some(value),
                    });
                    break;
                }
                out.push(Item::Opt { spec, value: None });
            }
            continue;
        }

        out.push(Item::Operand(token));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPECS: &[OptSpec] = &[
        short('l'),
        short('a'),
        short_long('h', "human"),
        short_value('n'),
        long("settings"),
    ];

    fn parse_ok<'a>(args: &[&'a str]) -> Vec<Item<'a>> {
        parse(args, SPECS).expect("must parse")
    }

    fn flags(items: &[Item<'_>]) -> Vec<char> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Opt {
                    spec:
                        OptSpec {
                            short: Some(ch), ..
                        },
                    ..
                } => Some(*ch),
                _ => None,
            })
            .collect()
    }

    fn operands<'a>(items: &[Item<'a>]) -> Vec<&'a str> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Operand(text) => Some(*text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn grouped_short_flags_split_into_options() {
        let items = parse_ok(&["-la", "/mnt"]);
        assert_eq!(flags(&items), alloc::vec!['l', 'a']);
        assert_eq!(operands(&items), alloc::vec!["/mnt"]);
    }

    #[test]
    fn double_dash_terminates_option_parsing() {
        let items = parse_ok(&["-l", "--", "-a", "--", "x"]);
        assert_eq!(flags(&items), alloc::vec!['l']);
        assert_eq!(operands(&items), alloc::vec!["-a", "--", "x"]);
    }

    #[test]
    fn single_dash_is_an_operand() {
        assert_eq!(operands(&parse_ok(&["-"])), alloc::vec!["-"]);
    }

    #[test]
    fn long_options_and_values() {
        let items = parse_ok(&["--human", "--settings", "run"]);
        assert_eq!(flags(&items), alloc::vec!['h']);
        assert_eq!(operands(&items), alloc::vec!["run"]);
        // Опция с аргументом: приклеенный и отдельный токен.
        let attached = parse_ok(&["-n5"]);
        let separate = parse_ok(&["-n", "5"]);
        assert_eq!(attached, separate);
        assert!(matches!(
            attached[0],
            Item::Opt {
                spec: _,
                value: Some("5")
            }
        ));
    }

    #[test]
    fn unknown_options_are_errors_not_silent_no_ops() {
        assert_eq!(parse(&["-x"], SPECS), Err(Error::UnknownShort('x')));
        assert_eq!(parse(&["--nope"], SPECS), Err(Error::UnknownLong("nope")));
        assert_eq!(parse(&["-lx"], SPECS), Err(Error::UnknownShort('x')));
        assert_eq!(
            parse(&["--human=yes"], SPECS),
            Err(Error::UnexpectedValue(&SPECS[2]))
        );
        assert_eq!(parse(&["-n"], SPECS), Err(Error::MissingValue(&SPECS[3])));
    }

    #[test]
    fn options_may_follow_operands_and_keep_operand_order() {
        let items = parse_ok(&["/mnt", "-l", "/mnt/etc", "-a"]);
        assert_eq!(flags(&items), alloc::vec!['l', 'a']);
        assert_eq!(operands(&items), alloc::vec!["/mnt", "/mnt/etc"]);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(parse(&[], SPECS).expect("must parse").is_empty());
    }
}
