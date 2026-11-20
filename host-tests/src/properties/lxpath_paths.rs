//! Host-свойства чистого PATH-поиска `lxrun` (`src/shell/lxpath.rs`) и
//! префиксного ELF-теста, которым поиск отбирает кандидата.
//!
//! Свойства фиксируют поведение, а не повторяют реализацию:
//!
//! * `search_dirs` — env-компоненты первыми, затем недостающие значения по
//!   умолчанию; пустые компоненты пропускаются, дубликаты схлопываются
//!   (проверяется независимым оракулом на случайных env-списках);
//! * `is_bare_name` — ровно правило `execvp`: `/` в любом месте означает путь,
//!   и такой токен никогда не ищется по PATH;
//! * `candidates` — по одному `dir/name` на непустой каталог, без `//` и с
//!   именем в конце; пустой каталог в списке не даёт кандидата;
//! * `display_dirs` — ровно `:`-склейка того же списка (диагностика not-found
//!   называет именно то, что искали);
//! * `looks_like_linux_elf` совпадает с полным `classify_elf` на 64-байтном
//!   заголовке (`phnum = 0`): префиксный тест принимает ровно те заголовки,
//!   которые загрузчик классифицировал бы как `ET_EXEC`/`ET_DYN` x86_64, и
//!   отвергает усечённые/чужие.

use crate::elf_classify::{classify_elf, looks_like_linux_elf, ElfVerdict};
use crate::lxpath::{
    candidates, display_dirs, is_bare_name, search_dirs, DEFAULT_DIRS, DEFAULT_PATH,
};
use proptest::prelude::*;

/// Каталоги, из которых составляются случайные env-списки. Пустая строка и
/// повторы включены намеренно: они проверяют пропуск пустых компонент и
/// схлопывание дубликатов.
const DIR_POOL: &[&str] = &[
    "",
    "/",
    "/mnt",
    "/mnt/usr/bin",
    "/usr/bin",
    "/bin",
    "/opt/bin",
    "rel/dir",
];

/// Независимый оракул: env-компоненты по порядку без пустых и повторов, затем
/// недостающие значения по умолчанию.
fn oracle_search_dirs(env: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(list) = env {
        for part in list.split(':') {
            if !part.is_empty() && !out.iter().any(|dir| dir == part) {
                out.push(part.to_string());
            }
        }
    }
    for dir in DEFAULT_DIRS {
        if !out.iter().any(|existing| existing == dir) {
            out.push(dir.to_string());
        }
    }
    out
}

fn sample_dirs(parts: &[&'static str]) -> Vec<&'static str> {
    // В `DIR_POOL` значения `&'static str`, а `prop::sample::select` возвращает
    // их же — так что список остаётся статическим для сигнатуры `&[&str]`.
    parts.to_vec()
}

/// Собрать 64-байтный ELF-заголовок с `phnum = 0` — минимальный образ, на
/// котором `classify_elf` проверяет только identity полей `Ehdr`.
fn minimal_header(e_type: u16, e_machine: u16, class: u8, data: u8) -> [u8; 64] {
    let mut header = [0u8; 64];
    header[0..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
    header[4] = class;
    header[5] = data;
    header[16..18].copy_from_slice(&e_type.to_le_bytes());
    header[18..20].copy_from_slice(&e_machine.to_le_bytes());
    header[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    header[56..58].copy_from_slice(&0u16.to_le_bytes()); // e_phnum = 0
    header
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// env-список только добавляет каталоги перед значениями по умолчанию:
    /// порядок и состав совпадают с независимым оракулом на любом наборе
    /// компонент (включая пустые и повторы).
    #[test]
    fn env_path_prepends_and_defaults_fill_the_gaps(
        parts in prop::collection::vec(prop::sample::select(DIR_POOL), 0..8)
    ) {
        let env = parts.join(":");
        let got: Vec<String> = search_dirs(Some(&env)).into_iter().map(String::from).collect();
        prop_assert_eq!(got, oracle_search_dirs(Some(&env)));
    }

    /// Значения по умолчанию всегда достижимы: их нельзя потерять из-за
    /// env-списка, каким бы тот ни был.
    #[test]
    fn default_dirs_are_always_searchable(
        parts in prop::collection::vec(prop::sample::select(DIR_POOL), 0..8)
    ) {
        let env = parts.join(":");
        let got = search_dirs(Some(&env));
        for dir in DEFAULT_DIRS {
            prop_assert!(got.contains(&dir), "default dir {} missing from {:?}", dir, got);
        }
        let mut unique: Vec<&str> = got.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(unique.len(), got.len());
    }

    /// `None`/пустая строка — ровно документированный PATH по умолчанию.
    #[test]
    fn unset_path_is_the_documented_default(_seed in any::<u8>()) {
        prop_assert_eq!(search_dirs(None), DEFAULT_DIRS.to_vec());
        prop_assert_eq!(search_dirs(Some("")), DEFAULT_DIRS.to_vec());
        prop_assert_eq!(display_dirs(&DEFAULT_DIRS), DEFAULT_PATH);
    }

    /// `/` в любом месте — это путь, а не имя: PATH-поиск к нему не применяется
    /// (иначе `./fastfetch` или `/bin/x` могли бы «найтись» в чужом каталоге).
    #[test]
    fn tokens_with_a_slash_are_paths_and_are_never_searched(
        name in "[A-Za-z0-9_.+@-]{0,8}/[A-Za-z0-9_.+@/-]{0,8}"
    ) {
        prop_assert!(!is_bare_name(&name));
        prop_assert!(candidates(&name, &DEFAULT_DIRS).is_empty());
    }

    /// Для голого имени — ровно один кандидат на непустой каталог, с именем в
    /// конце и без `//` (в том числе когда каталог — `/`).
    #[test]
    fn bare_names_join_one_candidate_per_nonempty_dir(
        name in "[A-Za-z0-9_.+@-]{1,12}",
        parts in prop::collection::vec(prop::sample::select(DIR_POOL), 0..6)
    ) {
        prop_assert!(is_bare_name(&name));
        let dirs = sample_dirs(&parts);
        let got = candidates(&name, &dirs);
        let want: Vec<String> = dirs
            .iter()
            .filter(|dir| !dir.is_empty())
            .map(|dir| {
                if dir.ends_with('/') {
                    std::format!("{}{}", dir, name)
                } else {
                    std::format!("{}/{}", dir, name)
                }
            })
            .collect();
        prop_assert_eq!(&got, &want);
        for candidate in &got {
            prop_assert!(!candidate.contains("//"), "double slash in {}", candidate);
            prop_assert!(candidate.ends_with(&name), "{} does not end with {}", candidate, name);
        }
    }

    /// Путь и пустое имя дают пустой список кандидатов — поиск не подменяет
    /// явно заданный путь.
    #[test]
    fn empty_name_and_absolute_path_have_no_candidates(
        absolute in "/[A-Za-z0-9_/]{1,16}",
    ) {
        prop_assert!(candidates("", &DEFAULT_DIRS).is_empty());
        prop_assert!(candidates(&absolute, &DEFAULT_DIRS).is_empty());
    }

    /// Диагностика not-found называет именно тот список, по которому шёл поиск.
    #[test]
    fn display_dirs_is_the_colon_join_of_the_search_list(
        parts in prop::collection::vec(prop::sample::select(DIR_POOL), 0..8)
    ) {
        let env = parts.join(":");
        let dirs = search_dirs(Some(&env));
        let expected: Vec<&str> = dirs.clone();
        prop_assert_eq!(display_dirs(&dirs), expected.join(":"));
    }

    /// Префиксный ELF-тест совпадает с полным классификатором на 64-байтном
    /// заголовке: принимается ровно `ET_EXEC`/`ET_DYN` x86_64 little-endian
    /// ELF64 с валидной магией.
    #[test]
    fn elf_header_prefilter_agrees_with_classifier(
        magic_ok in any::<bool>(),
        class in any::<u8>(),
        data in any::<u8>(),
        machine in any::<u16>(),
        e_type in any::<u16>(),
    ) {
        let mut header = minimal_header(e_type, machine, class, data);
        if !magic_ok {
            header[0] = 0;
        }
        let loadable = matches!(classify_elf(&header), ElfVerdict::Load { .. });
        prop_assert_eq!(looks_like_linux_elf(&header), loadable);
    }

    /// Усечённый заголовок (меньше 64 байт) отвергается: поиск не примет
    /// обрезанный файл за Linux ELF.
    #[test]
    fn truncated_headers_are_rejected(len in 0usize..64) {
        let header = minimal_header(2, 0x3E, 2, 1);
        prop_assert!(!looks_like_linux_elf(&header[..len]));
    }

    /// Валидные ET_EXEC и ET_DYN принимаются независимо от байтов после
    /// заголовка (хвост образа префиксный тест не читает).
    #[test]
    fn exec_and_dyn_headers_are_accepted(
        e_type in prop_oneof![Just(2u16), Just(3u16)],
        tail in prop::collection::vec(any::<u8>(), 0..16),
    ) {
        let header = minimal_header(e_type, 0x3E, 2, 1);
        prop_assert!(looks_like_linux_elf(&header));
        let mut with_tail = header.to_vec();
        with_tail.extend_from_slice(&tail);
        prop_assert!(looks_like_linux_elf(&with_tail));
    }
}