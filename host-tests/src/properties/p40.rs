// Feature: full-debian-apt-update, Property 7 (Validates R8.1, R8.4): the
// dependency resolver `apt_resolve::resolve_install`, run over the compact
// `PackageIndex`, produces exactly the install plan an independent reference
// resolver produces over a straightforward `String`-keyed model built from the
// same parsed records.
//
// The compact `PackageIndex` is now the only index, so this property pins the
// resolver's documented semantics (post-order DFS, OR-group alternative choice,
// already-installed pruning, virtual-name resolution, NotFound only for an
// unsatisfiable target) against a transparent reimplementation in the test.
//
// Reference model (mirrors p35):
//   * `by_name`: file order, last record wins on a duplicate `Package`.
//   * `by_provides`: file order, a provided name only if it is NOT a real
//     package name and not yet recorded (first provider wins).
//   * `get_provider(name)`: real precedence, then first-provider-wins.
//   * `contains(name)`: real OR provided.
//
// Reference resolver (mirrors src/pkg/apt_resolve.rs EXACTLY):
//   * target already installed -> empty plan.
//   * target not satisfiable (`get_provider` is None) and not installed -> NotFound.
//   * otherwise a post-order DFS keyed by resolved real package name, tracking
//     visited / on_stack; for each AND-group choose the first already-installed
//     alternative, else the first alternative present in the index, else skip the
//     group; recurse on the chosen alt unless it is already installed; push the
//     real name post-order. The plan is dependency-first.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PackageIndex, PkgRecord};
use crate::apt_resolve::{resolve_install, AptError};
use proptest::prelude::*;

/// Build a random but well-formed `Packages` document from `n` stanzas. Field
/// presence and ordering vary so the parser's optional-field and
/// continuation-line handling is exercised. (Copied from p33 — per-file copying
/// matches the existing property-test style.)
fn packages_doc_strategy() -> impl Strategy<Value = String> {
    let name = "[a-z][a-z0-9.+-]{0,12}";
    let ver = "[0-9]{1,2}\\.[0-9]{1,3}(-[0-9]{1,2})?";
    let stanza = (
        name,
        ver,
        prop::option::of("[a-z0-9 ,|()>=.:+-]{0,40}"), // Depends
        prop::option::of("[a-z0-9 ,.+-]{0,30}"),        // Provides
        prop::option::of(0u64..9_000_000),              // Size
        any::<bool>(),                                   // include a continuation Description
    )
        .prop_map(|(pkg, version, deps, provides, size, desc)| {
            let mut s = String::new();
            s.push_str("Package: ");
            s.push_str(&pkg);
            s.push('\n');
            s.push_str("Version: ");
            s.push_str(&version);
            s.push('\n');
            s.push_str("Architecture: amd64\n");
            s.push_str("Filename: pool/main/x/");
            s.push_str(&pkg);
            s.push_str(".deb\n");
            if let Some(d) = deps {
                s.push_str("Depends: ");
                s.push_str(&d);
                s.push('\n');
            }
            if let Some(p) = provides {
                s.push_str("Provides: ");
                s.push_str(&p);
                s.push('\n');
            }
            if desc {
                s.push_str("Description: short\n a continued description line\n another line\n");
            }
            if let Some(sz) = size {
                s.push_str("Size: ");
                s.push_str(&sz.to_string());
                s.push('\n');
            }
            s
        });
    prop::collection::vec(stanza, 0..8).prop_map(|stanzas| stanzas.join("\n"))
}

/// The reference query model: name -> record index (last-wins) and provided name
/// -> record index (first-provider-wins, real names excluded). Mirrors p35.
struct RefModel {
    records: Vec<PkgRecord>,
    by_name: BTreeMap<String, usize>,
    by_provides: BTreeMap<String, usize>,
}

impl RefModel {
    fn build(records: Vec<PkgRecord>) -> Self {
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for (i, rec) in records.iter().enumerate() {
            by_name.insert(rec.package.clone(), i);
        }
        let mut by_provides: BTreeMap<String, usize> = BTreeMap::new();
        for (i, rec) in records.iter().enumerate() {
            for p in &rec.provides {
                if by_name.contains_key(p) {
                    continue;
                }
                by_provides.entry(p.clone()).or_insert(i);
            }
        }
        RefModel {
            records,
            by_name,
            by_provides,
        }
    }

    /// `get_provider(name)` -> record index: real precedence, then provides.
    fn get_provider(&self, name: &str) -> Option<usize> {
        if let Some(&i) = self.by_name.get(name) {
            return Some(i);
        }
        self.by_provides.get(name).copied()
    }

    /// `contains(name)`: real OR provided.
    fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name) || self.by_provides.contains_key(name)
    }
}

/// Reference reimplementation of `apt_resolve::choose_alternative`: the first
/// already-installed alternative, else the first present in the index, else None.
fn ref_choose(model: &RefModel, alts: &[String], installed: &BTreeSet<String>) -> Option<String> {
    for alt in alts {
        if installed.contains(alt) {
            return Some(alt.clone());
        }
    }
    for alt in alts {
        if model.contains(alt) {
            return Some(alt.clone());
        }
    }
    None
}

/// Reference reimplementation of `apt_resolve::visit`: post-order DFS keyed by
/// the resolved real package name.
fn ref_visit(
    model: &RefModel,
    name: &str,
    installed: &BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    on_stack: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) {
    let idx = match model.get_provider(name) {
        Some(i) => i,
        None => return,
    };
    let real_name = model.records[idx].package.clone();

    if installed.contains(&real_name) {
        return;
    }
    if visited.contains(&real_name) || on_stack.contains(&real_name) {
        return;
    }

    on_stack.insert(real_name.clone());

    for group in &model.records[idx].depends {
        let alts: Vec<String> = group.alts.clone();
        if let Some(chosen) = ref_choose(model, &alts, installed) {
            if !installed.contains(&chosen) {
                ref_visit(model, &chosen, installed, visited, on_stack, order);
            }
        }
    }

    on_stack.remove(&real_name);
    if visited.insert(real_name.clone()) {
        order.push(real_name);
    }
}

/// Reference reimplementation of `apt_resolve::resolve_install`. Returns
/// `Err(())` to denote the `AptError::NotFound` case (target unsatisfiable and
/// not already installed).
fn ref_resolve(
    model: &RefModel,
    target: &str,
    installed: &BTreeSet<String>,
) -> Result<Vec<String>, ()> {
    if installed.contains(target) {
        return Ok(Vec::new());
    }
    if model.get_provider(target).is_none() {
        return Err(());
    }
    let mut order: Vec<String> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut on_stack: BTreeSet<String> = BTreeSet::new();
    ref_visit(model, target, installed, &mut visited, &mut on_stack, &mut order);
    Ok(order)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Feature: full-debian-apt-update, Property 7: `resolve_install` over the
    /// compact `PackageIndex` matches the reference resolver's plan exactly — same
    /// `NotFound` outcome for an unsatisfiable target, and otherwise the same plan
    /// VECTOR (same order, same elements).
    #[test]
    fn resolver_plan_equivalence_over_compact_index(
        doc in packages_doc_strategy(),
        mode in 0u8..3,
        pick in 0usize..1024,
        absent in "[a-z0-9.+:_-]{0,14}",
        inst_mask in prop::collection::vec(any::<bool>(), 0..16),
    ) {
        let records = parse_packages(doc.as_bytes());
        let model = RefModel::build(records.clone());
        let index = PackageIndex::from_records(records);

        // The set of real package names (file order, unique) and provided names.
        let real_names: Vec<String> = model.by_name.keys().cloned().collect();
        let prov_names: Vec<String> = model.by_provides.keys().cloned().collect();

        // `already_installed`: a subset of the doc's real names selected by the
        // (cycled) random mask.
        let installed: BTreeSet<String> = if inst_mask.is_empty() {
            BTreeSet::new()
        } else {
            real_names
                .iter()
                .enumerate()
                .filter(|(i, _)| inst_mask[i % inst_mask.len()])
                .map(|(_, n)| n.clone())
                .collect()
        };

        // Draw the target from {a real name, a provided name, a random absent
        // string}, falling back to the random string when the chosen pool is empty.
        let target: String = match mode {
            0 if !real_names.is_empty() => real_names[pick % real_names.len()].clone(),
            1 if !prov_names.is_empty() => prov_names[pick % prov_names.len()].clone(),
            _ => absent.clone(),
        };

        let reference = ref_resolve(&model, &target, &installed);
        let actual = resolve_install(&index, &target, &installed);

        match reference {
            Err(()) => {
                // Unsatisfiable target -> resolver must return NotFound(target).
                match actual {
                    Err(AptError::NotFound(n)) => prop_assert_eq!(n, target.clone()),
                    Ok(plan) => prop_assert!(
                        false,
                        "expected NotFound({:?}), got Ok({:?})",
                        target,
                        plan
                    ),
                }
            }
            Ok(expected_plan) => {
                let plan = actual.expect("satisfiable target must resolve to a plan");
                // Same order, same elements.
                prop_assert_eq!(plan, expected_plan);
            }
        }
    }
}
