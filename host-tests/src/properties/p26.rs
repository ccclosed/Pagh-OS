// Feature: linux-binary-compat, Property 26: package installation preserves safe regular-file content and skips unsafe/irregular entries

use crate::install::{install_model, normalize_entry_path, NormPath};
use crate::tar::{TarEntry, TarType};
use proptest::prelude::*;
use std::collections::BTreeMap;

/// Owned backing data for a generated tar entry. `TarEntry` borrows its `path`
/// and `content`, so the owned values must outlive the borrowed view.
#[derive(Clone, Debug)]
struct OwnedEntry {
    path: String,
    kind: TarType,
    content: Vec<u8>,
}

fn kind_strategy() -> impl Strategy<Value = TarType> {
    prop_oneof![
        Just(TarType::Regular),
        Just(TarType::Directory),
        Just(TarType::Other),
    ]
}

/// Generate archived paths mixing safe relative paths, leading `./` and `/`
/// prefixes, and `..`/`.` components that may escape the root.
fn path_strategy() -> impl Strategy<Value = String> {
    let component = prop_oneof![
        Just("a".to_string()),
        Just("b".to_string()),
        Just("dir".to_string()),
        Just("..".to_string()),
        Just(".".to_string()),
    ];
    (
        prop::option::of(prop_oneof![Just("./".to_string()), Just("/".to_string())]),
        prop::collection::vec(component, 0..5),
    )
        .prop_map(|(prefix, comps)| {
            let mut s = String::new();
            if let Some(p) = prefix {
                s.push_str(&p);
            }
            s.push_str(&comps.join("/"));
            s
        })
}

fn entry_strategy() -> impl Strategy<Value = OwnedEntry> {
    (
        path_strategy(),
        kind_strategy(),
        prop::collection::vec(any::<u8>(), 0..32),
    )
        .prop_map(|(path, kind, content)| OwnedEntry { path, kind, content })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `install_model` yields exactly the regular files whose normalized path is
    /// `Keep`, with content preserved and last-writer-wins on duplicate paths;
    /// directories, other kinds, and unsafe paths are absent.
    #[test]
    fn install_model_keeps_safe_regular_files(
        owned in prop::collection::vec(entry_strategy(), 0..12),
    ) {
        let entries: Vec<TarEntry> = owned
            .iter()
            .map(|o| TarEntry {
                path: &o.path,
                kind: o.kind,
                mode: 0,
                size: o.content.len() as u64,
                content: &o.content,
            })
            .collect();

        let result = install_model(&entries);

        // Every key in the model is itself a safe, normalized path (idempotent):
        // it cannot start with `/` or `./`, nor escape via `..`.
        for k in result.keys() {
            prop_assert_eq!(normalize_entry_path(k), NormPath::Keep(k.clone()));
        }

        // Independently fold the documented selection rules: regular files only,
        // safe normalized path, content preserved, last writer wins.
        let mut expected: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for o in &owned {
            if o.kind == TarType::Regular {
                if let NormPath::Keep(p) = normalize_entry_path(&o.path) {
                    expected.insert(p, o.content.clone());
                }
            }
        }

        prop_assert_eq!(result, expected);
    }
}
