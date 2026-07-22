// Feature: linux-binary-compat, Property 31: the apt `Packages` index parser and
// the dependency resolver. Re-created after a filename collision moved the DNS
// tests into p30.rs.
//
// Covers:
//   * parse_packages on a realistic multi-stanza fixture (continuation lines,
//     Depends with `|` alternatives and version constraints, a Provides);
//   * parse_depends edge cases;
//   * resolve_install dependency-first ordering, missing-essential skipping,
//     unknown-target NotFound, already-installed exclusion, virtual-via-provides;
//   * a random-DAG property: every dependency precedes its dependent.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_depends, parse_packages, DepGroup, PackageIndex, PkgRecord};
use crate::apt_resolve::{resolve_install, AptError};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a synthetic record. `deps` is a list of AND-groups, each a list of
/// OR-ed bare names; `provides` is a list of virtual names.
fn rec(name: &str, deps: &[&[&str]], provides: &[&str]) -> PkgRecord {
    PkgRecord {
        package: name.to_string(),
        version: "1.0".to_string(),
        arch: "amd64".to_string(),
        filename: alloc::format!("pool/main/{}/{}_1.0_amd64.deb", &name[..1], name),
        depends: deps
            .iter()
            .map(|g| DepGroup {
                alts: g.iter().map(|s| s.to_string()).collect(),
            })
            .collect(),
        provides: provides.iter().map(|s| s.to_string()).collect(),
        size: 0,
    }
}

fn empty_set() -> BTreeSet<String> {
    BTreeSet::new()
}

fn pos(plan: &[String], name: &str) -> Option<usize> {
    plan.iter().position(|p| p == name)
}

// ---------------------------------------------------------------------------
// parse_packages on a realistic fixture
// ---------------------------------------------------------------------------

const FIXTURE: &str = "\
Package: hello
Version: 2.10-3
Architecture: amd64
Filename: pool/main/h/hello/hello_2.10-3_amd64.deb
Depends: libc6 (>= 2.34), foo | bar (>= 1.0)
Description: a friendly greeting
 This is a continuation line that extends the description field
 across several physical lines.
Size: 53000

Package: libc6
Version: 2.36-9
Architecture: amd64
Filename: pool/main/g/glibc/libc6_2.36-9_amd64.deb
Provides: libc-l10n, glibc
Pre-Depends: libgcc-s1
Size: 2800000

Package: bar
Version: 1.2
Architecture: amd64
Filename: pool/main/b/bar/bar_1.2_amd64.deb
Size: 1000
";

#[test]
fn parse_fixture_fields() {
    // CRLF tolerance: feed the fixture with CRLF endings too.
    let crlf = FIXTURE.replace('\n', "\r\n");
    for text in [FIXTURE.as_bytes(), crlf.as_bytes()] {
        let records = parse_packages(text);
        assert_eq!(records.len(), 3, "expected 3 stanzas");

        let hello = &records[0];
        assert_eq!(hello.package, "hello");
        assert_eq!(hello.version, "2.10-3");
        assert_eq!(hello.arch, "amd64");
        assert_eq!(
            hello.filename,
            "pool/main/h/hello/hello_2.10-3_amd64.deb"
        );
        assert_eq!(hello.size, 53000);
        // Depends: two AND-groups. First is libc6 (constraint stripped);
        // second is `foo | bar` (two alternatives, constraint stripped).
        assert_eq!(hello.depends.len(), 2);
        assert_eq!(hello.depends[0].alts, ["libc6"]);
        assert_eq!(hello.depends[1].alts, ["foo", "bar"]);

        let libc6 = &records[1];
        assert_eq!(libc6.package, "libc6");
        // Pre-Depends folds into depends.
        assert_eq!(libc6.depends.len(), 1);
        assert_eq!(libc6.depends[0].alts, ["libgcc-s1"]);
        // Provides parsed into bare virtual names.
        assert_eq!(libc6.provides, ["libc-l10n", "glibc"]);
        assert_eq!(libc6.size, 2800000);
    }
}

#[test]
fn stanza_without_package_is_skipped() {
    let text = "Version: 1.0\nArchitecture: amd64\n\nPackage: real\nVersion: 2.0\n";
    let records = parse_packages(text.as_bytes());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].package, "real");
}

#[test]
fn index_lookup_real_and_virtual() {
    let records = parse_packages(FIXTURE.as_bytes());
    let index = PackageIndex::from_records(records);

    assert!(index.get("hello").is_some());
    assert!(index.get("libc6").is_some());
    // Virtual name resolves to its provider (libc6).
    assert!(index.get("glibc").is_none(), "glibc is virtual, not real");
    assert_eq!(index.get_provider("glibc").unwrap().package(), "libc6");
    assert!(index.contains("libc-l10n"));
    // names() is sorted + unique.
    let names: Vec<&str> = index.names().collect();
    assert_eq!(names, ["bar", "hello", "libc6"]);
}

// ---------------------------------------------------------------------------
// parse_depends edge cases
// ---------------------------------------------------------------------------

#[test]
fn parse_depends_edge_cases() {
    // Empty / whitespace -> no groups.
    assert!(parse_depends("").is_empty());
    assert!(parse_depends("   ").is_empty());

    // Version constraints and arch qualifiers are stripped.
    let g = parse_depends("libc6 (>= 2.34), libfoo:any, zlib1g (>= 1:1.2)");
    assert_eq!(g.len(), 3);
    assert_eq!(g[0].alts, ["libc6"]);
    assert_eq!(g[1].alts, ["libfoo"]);
    assert_eq!(g[2].alts, ["zlib1g"]);

    // OR alternatives within one AND-group.
    let g = parse_depends("default-mta | mail-transport-agent");
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].alts, ["default-mta", "mail-transport-agent"]);

    // Trailing/double commas and empty alternatives are dropped, not panicked.
    let g = parse_depends("a, , b |, | c");
    assert_eq!(g.len(), 3);
    assert_eq!(g[0].alts, ["a"]);
    assert_eq!(g[1].alts, ["b"]);
    assert_eq!(g[2].alts, ["c"]);
}

// ---------------------------------------------------------------------------
// resolve_install behaviors
// ---------------------------------------------------------------------------

/// Synthetic index: A depends on B and C; C depends on D. B and D are leaves.
fn abcd_index() -> PackageIndex {
    PackageIndex::from_records(alloc::vec![
        rec("a", &[&["b"], &["c"]], &[]),
        rec("b", &[], &[]),
        rec("c", &[&["d"]], &[]),
        rec("d", &[], &[]),
    ])
}

#[test]
fn resolve_dependency_first_order() {
    let index = abcd_index();
    let plan = resolve_install(&index, "a", &empty_set()).unwrap();

    // All four are scheduled, target last among its subtree.
    assert_eq!(plan.len(), 4);
    assert_eq!(plan.last().unwrap(), "a");

    // Each dependency precedes its dependent.
    assert!(pos(&plan, "b").unwrap() < pos(&plan, "a").unwrap());
    assert!(pos(&plan, "c").unwrap() < pos(&plan, "a").unwrap());
    assert!(pos(&plan, "d").unwrap() < pos(&plan, "c").unwrap());
}

#[test]
fn resolve_skips_missing_essential_dependency() {
    // `a` depends on `libc6`, which is NOT in the index (assumed essential).
    let index = PackageIndex::from_records(alloc::vec![rec("a", &[&["libc6"]], &[])]);
    let plan = resolve_install(&index, "a", &empty_set()).unwrap();
    // No error; the missing dep is silently skipped, only `a` is installed.
    assert_eq!(plan, ["a"]);
}

#[test]
fn resolve_unknown_target_is_not_found() {
    let index = abcd_index();
    match resolve_install(&index, "nope", &empty_set()) {
        Err(AptError::NotFound(n)) => assert_eq!(n, "nope"),
        other => panic!("expected NotFound, got {:?}", other),
    }
}

#[test]
fn resolve_excludes_already_installed() {
    let index = abcd_index();

    // If C and D are already installed, only B and A remain.
    let mut installed = empty_set();
    installed.insert("c".to_string());
    installed.insert("d".to_string());
    let plan = resolve_install(&index, "a", &installed).unwrap();
    assert_eq!(plan, ["b", "a"]);

    // If the target itself is installed, the plan is empty.
    let mut all = empty_set();
    all.insert("a".to_string());
    assert!(resolve_install(&index, "a", &all).unwrap().is_empty());
}

#[test]
fn resolve_virtual_via_provides() {
    // `app` depends on the virtual `httpd`; `nginx` provides it.
    let index = PackageIndex::from_records(alloc::vec![
        rec("app", &[&["httpd"]], &[]),
        rec("nginx", &[], &["httpd"]),
    ]);
    let plan = resolve_install(&index, "app", &empty_set()).unwrap();
    // The providing real package (nginx) is scheduled before app.
    assert_eq!(plan, ["nginx", "app"]);

    // Installing the virtual name directly also resolves to the provider.
    let plan = resolve_install(&index, "httpd", &empty_set()).unwrap();
    assert_eq!(plan, ["nginx"]);
}

#[test]
fn resolve_alternative_prefers_installed() {
    // `a` depends on `x | y`. With y already installed, the group is satisfied
    // and neither x nor y is (re)scheduled.
    let index = PackageIndex::from_records(alloc::vec![
        rec("a", &[&["x", "y"]], &[]),
        rec("x", &[], &[]),
        rec("y", &[], &[]),
    ]);
    let mut installed = empty_set();
    installed.insert("y".to_string());
    let plan = resolve_install(&index, "a", &installed).unwrap();
    assert_eq!(plan, ["a"]);
    assert!(pos(&plan, "x").is_none());
}

// ---------------------------------------------------------------------------
// Random-DAG property: every dependency precedes its dependent
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Build a random DAG over nodes p0..p{n-1} where each node may only depend
    /// on higher-indexed nodes (guaranteeing acyclicity). Resolving from p0, for
    /// every package in the plan, each of its dependencies that is also in the
    /// plan must appear strictly earlier. The plan is also duplicate-free.
    #[test]
    fn random_dag_dependency_precedes_dependent(
        // For nodes 1..n, a bitmask of which strictly-higher nodes it depends on.
        edges in prop::collection::vec(any::<u16>(), 4..12usize),
    ) {
        let n = edges.len();

        // Build records: node i depends (as separate AND-groups) on each j > i
        // whose bit (j - i - 1) is set in edges[i].
        let mut records: Vec<PkgRecord> = Vec::with_capacity(n);
        for i in 0..n {
            let mut groups: Vec<DepGroup> = Vec::new();
            for j in (i + 1)..n {
                let bit = j - i - 1;
                if bit < 16 && (edges[i] >> bit) & 1 == 1 {
                    groups.push(DepGroup {
                        alts: alloc::vec![alloc::format!("p{}", j)],
                    });
                }
            }
            records.push(PkgRecord {
                package: alloc::format!("p{}", i),
                version: "1".to_string(),
                arch: "amd64".to_string(),
                filename: alloc::format!("pool/p{}.deb", i),
                depends: groups,
                provides: Vec::new(),
                size: 0,
            });
        }

        let index = PackageIndex::from_records(records.clone());
        let plan = resolve_install(&index, "p0", &empty_set()).unwrap();

        // Duplicate-free.
        let set: BTreeSet<&String> = plan.iter().collect();
        prop_assert_eq!(set.len(), plan.len());

        // Target present and last among its subtree.
        prop_assert!(pos(&plan, "p0").is_some());
        prop_assert_eq!(plan.last().unwrap(), "p0");

        // For each planned package, every in-plan dependency precedes it.
        for rec in &records {
            if let Some(pi) = pos(&plan, &rec.package) {
                for group in &rec.depends {
                    for alt in &group.alts {
                        if let Some(di) = pos(&plan, alt) {
                            prop_assert!(di < pi, "{} should precede {}", alt, rec.package);
                        }
                    }
                }
            }
        }
    }
}
