// Feature: full-debian-apt-update, Property 2 (Validates R1.1, R1.2, R1.3,
// R6.3): the compact `PackageIndex` query surface is observationally equivalent
// to a straightforward `String`-keyed reference model built from the same parsed
// records.
//
// The reference model captures the documented lookup semantics:
//   * `by_name`: a `BTreeMap<String, usize>` filled in file order, so a duplicate
//     `Package` name overwrites — last-record-wins (R1.1/R1.2 real lookup).
//   * `by_provides`: a `BTreeMap<String, usize>` filled in file order, recording a
//     provided (virtual) name only if it is NOT a real package name and not yet
//     recorded — real names take precedence and the first provider wins (R1.3).
//
// For every real name, every provided name, and a handful of random absent names
// we compare `get`, `get_provider`, and `contains` against the model; we also
// check `len`/`is_empty` and that `names()` equals the sorted+deduped real names.
// `PkgRef` has no `PartialEq`, so every comparison is on the package-name STRING.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PackageIndex, PkgRecord};
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

/// The reference query model: name -> record index (last-wins), provided name ->
/// record index (first-provider-wins, real names excluded).
struct RefModel {
    records: Vec<PkgRecord>,
    by_name: BTreeMap<String, usize>,
    by_provides: BTreeMap<String, usize>,
}

impl RefModel {
    fn build(records: Vec<PkgRecord>) -> Self {
        // by_name: file order, last write wins on duplicate Package.
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for (i, rec) in records.iter().enumerate() {
            by_name.insert(rec.package.clone(), i);
        }
        // by_provides: file order; record a provided name only if it is not a
        // real package name and not already recorded (first provider wins).
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

    /// Expected `get(name)` package-name string (real lookup, last-wins).
    fn expect_get(&self, name: &str) -> Option<String> {
        self.by_name
            .get(name)
            .map(|&i| self.records[i].package.clone())
    }

    /// Expected `get_provider(name)` package-name string (real precedence, then
    /// first-provider-wins).
    fn expect_provider(&self, name: &str) -> Option<String> {
        if let Some(&i) = self.by_name.get(name) {
            return Some(self.records[i].package.clone());
        }
        self.by_provides
            .get(name)
            .map(|&i| self.records[i].package.clone())
    }

    /// Expected `contains(name)`.
    fn expect_contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name) || self.by_provides.contains_key(name)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Property 2: the compact index answers `get`/`get_provider`/`contains`/
    /// `len`/`is_empty`/`names` identically to the reference `String` model.
    #[test]
    fn query_equivalence_vs_reference_model(
        doc in packages_doc_strategy(),
        probes in prop::collection::vec("[a-z0-9.+:_-]{0,14}", 0..8),
    ) {
        let records = parse_packages(doc.as_bytes());
        let model = RefModel::build(records.clone());
        let index = PackageIndex::from_records(records);

        // len / is_empty mirror the record count.
        prop_assert_eq!(index.len(), model.records.len());
        prop_assert_eq!(index.is_empty(), model.records.is_empty());

        // names(): sorted + deduped real package names.
        let mut expected_names: Vec<String> =
            model.by_name.keys().cloned().collect(); // BTreeMap keys already sorted+unique
        expected_names.sort();
        let actual_names: Vec<String> =
            index.names().map(|s| s.to_string()).collect();
        prop_assert_eq!(actual_names, expected_names);

        // Assemble the set of names to probe: every real name, every provided
        // name, and the random (mostly absent) probe strings.
        let mut to_check: BTreeSet<String> = BTreeSet::new();
        for n in model.by_name.keys() {
            to_check.insert(n.clone());
        }
        for n in model.by_provides.keys() {
            to_check.insert(n.clone());
        }
        for p in &probes {
            to_check.insert(p.clone());
        }

        for n in &to_check {
            // get(): compare package-name string (PkgRef has no PartialEq).
            let got = index.get(n).map(|r| r.package().to_string());
            prop_assert_eq!(got, model.expect_get(n));

            // get_provider(): real precedence then first-provider-wins.
            let prov = index.get_provider(n).map(|r| r.package().to_string());
            prop_assert_eq!(prov, model.expect_provider(n));

            // contains(): real or provided.
            prop_assert_eq!(index.contains(n), model.expect_contains(n));
        }
    }
}
