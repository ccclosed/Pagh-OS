// Feature: ext2 links (issue #18) — host properties for path resolution
// (`src/vfs/link_walk.rs`): the component walk, `..` handling, relative and
// absolute link targets, the `SYMLOOP_MAX` budget and the guest-namespace
// mapping of link targets (contract `EXT2-LINKS.md` §4.6, §7.4, §8.7).
//
// Two layers of evidence:
//
//   * hand-computed Linux expectations for the cases that are easy to get
//     subtly wrong (`/a/link/..` after an absolute jump, a relative target with
//     `..`, a dangling link under `lstat` vs `stat`, cycles, the exact budget
//     boundary, `/..` at the root);
//   * a randomized comparison against an **independent oracle** — a
//     restart-based resolver written from the Linux algorithm in this file,
//     deliberately not sharing code with the module's in-place splice.

use crate::link_walk::{
    components, guest_path_keeps_root, map_guest_target, resolve, LinkTarget, LinkTree, WalkError,
    MAX_EXPANDED_BYTES, SYMLOOP_MAX,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

// ───────────────────────── a tiny link-aware tree ─────────────────────────

#[derive(Clone, Debug)]
enum Kind {
    Dir,
    File,
    Link(String),
}

#[derive(Clone, Debug)]
struct Node {
    kind: Kind,
    children: BTreeMap<String, usize>,
}

#[derive(Clone, Debug)]
struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    fn new() -> Tree {
        Tree {
            nodes: vec![Node {
                kind: Kind::Dir,
                children: BTreeMap::new(),
            }],
        }
    }

    fn id(&self, path: &str) -> Option<usize> {
        let mut id = 0usize;
        for comp in path.split('/') {
            if comp.is_empty() || comp == "." {
                continue;
            }
            id = *self.nodes[id].children.get(comp)?;
        }
        Some(id)
    }

    /// Create `path` (missing parents become directories) with the given kind.
    fn make(&mut self, path: &str, kind: Kind) -> usize {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let mut id = 0usize;
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let existing = self.nodes[id].children.get(*comp).copied();
            id = match existing {
                Some(child) => child,
                None => {
                    let child = self.nodes.len();
                    self.nodes.push(Node {
                        kind: if last { kind.clone() } else { Kind::Dir },
                        children: BTreeMap::new(),
                    });
                    self.nodes[id].children.insert((*comp).to_string(), child);
                    child
                }
            };
        }
        id
    }
}

impl LinkTree for Tree {
    fn root(&self) -> usize {
        0
    }
    fn lookup(&mut self, id: usize, name: &str) -> Option<usize> {
        self.nodes[id].children.get(name).copied()
    }
    fn is_dir(&self, id: usize) -> bool {
        matches!(self.nodes[id].kind, Kind::Dir)
    }
    fn link_target(&self, id: usize) -> Option<LinkTarget> {
        match &self.nodes[id].kind {
            Kind::Link(target) => Some(map_guest_target(target)),
            _ => None,
        }
    }
}

/// A tree with the shapes the hand-written cases need.
///
/// Everything lives under `mnt/`, because that is where the kernel mounts ext2:
/// a link target written as `/b/c` is a *guest* path and the walker maps it to
/// `/mnt/b/c` (exactly like the syscall layer maps a syscall path).
fn fixture() -> Tree {
    let mut t = Tree::new();
    t.make("mnt/a", Kind::Dir);
    t.make("mnt/a/f", Kind::File);
    t.make("mnt/b/c/f", Kind::File);
    t.make("mnt/x/y", Kind::File);
    // `/mnt/a/link` → guest `/b/c`, i.e. `/mnt/b/c`: `..` after the jump belongs
    // to `/mnt/b`.
    t.make("mnt/a/link", Kind::Link("/b/c".to_string()));
    // `/mnt/a/rel` → relative `../x`: resolved against `/mnt/a`, i.e. `/mnt/x`.
    t.make("mnt/a/rel", Kind::Link("../x".to_string()));
    // `/mnt/a/deep` → relative `../x/../x/y`: `..` crumbs must be walked, not
    // collapsed lexically against the link's own directory.
    t.make("mnt/a/deep", Kind::Link("../x/../x/y".to_string()));
    t.make("mnt/a/dang", Kind::Link("/nope".to_string()));
    t.make("mnt/a/loop1", Kind::Link("/a/loop2".to_string()));
    t.make("mnt/a/loop2", Kind::Link("/a/loop1".to_string()));
    t.make("mnt/a/self", Kind::Link("/a/self".to_string()));
    t.make("mnt/a/empty", Kind::Link(String::new()));
    t
}

fn walk(t: &mut Tree, path: &str, follow: bool) -> Result<usize, WalkError> {
    resolve(t, path, follow)
}

// ───────────────────── hand-computed Linux expectations ─────────────────────

#[test]
fn absolute_target_restarts_at_the_root() {
    let mut t = fixture();
    let f = t.id("mnt/b/c/f").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/link/f", true), Ok(f));
    // `..` applies to the *target's* directory, not to the link's.
    let b = t.id("mnt/b").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/link/..", true), Ok(b));
    let c = t.id("mnt/b/c").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/link/../c", true), Ok(c));
}

#[test]
fn relative_target_resolves_against_the_links_directory() {
    let mut t = fixture();
    let y = t.id("mnt/x/y").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/rel/y", true), Ok(y));
    // `../x/../x/y` from `/mnt/a` → `/mnt/x/y`.
    assert_eq!(walk(&mut t, "/mnt/a/deep", true), Ok(y));
    // Resolving the target against the *cwd* (the walker's root here) would look
    // for a top-level `/x`, which the tree does not have.
    assert_eq!(walk(&mut t, "/x/y", true), Err(WalkError::NotFound));
}

#[test]
fn final_link_followed_only_when_requested() {
    let mut t = fixture();
    let link = t.id("mnt/a/link").unwrap();
    // `lstat`/`readlink` see the link itself…
    assert_eq!(walk(&mut t, "/mnt/a/link", false), Ok(link));
    // …while `stat`/`open` reach the target.
    let c = t.id("mnt/b/c").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/link", true), Ok(c));
    // Intermediates are followed even with `follow_final == false`.
    let f = t.id("mnt/b/c/f").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/link/f", false), Ok(f));
}

#[test]
fn dangling_and_empty_targets_are_enoent() {
    let mut t = fixture();
    assert_eq!(walk(&mut t, "/mnt/a/dang", true), Err(WalkError::NotFound));
    // `lstat` of the dangling link itself succeeds.
    let dang = t.id("mnt/a/dang").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/dang", false), Ok(dang));
    // A link whose target cannot be read must never resolve to the link node
    // when the link is followed…
    assert_eq!(walk(&mut t, "/mnt/a/empty", true), Err(WalkError::NotFound));
    // …while `lstat`/`readlink` (follow_final == false) never look at the target
    // and still see the link itself.
    let empty = t.id("mnt/a/empty").unwrap();
    assert_eq!(walk(&mut t, "/mnt/a/empty", false), Ok(empty));
}

#[test]
fn cycles_hit_the_budget() {
    let mut t = fixture();
    assert_eq!(
        walk(&mut t, "/mnt/a/loop1", true),
        Err(WalkError::TooManyLinks)
    );
    assert_eq!(
        walk(&mut t, "/mnt/a/self", true),
        Err(WalkError::TooManyLinks)
    );
    // Self-loop through an intermediate component, too.
    assert_eq!(
        walk(&mut t, "/mnt/a/loop1/f", true),
        Err(WalkError::TooManyLinks)
    );
}

#[test]
fn dots_and_root_edges() {
    let mut t = fixture();
    assert_eq!(walk(&mut t, "/", true), Ok(0));
    assert_eq!(walk(&mut t, "/..", true), Ok(0));
    assert_eq!(walk(&mut t, "/mnt/a/../..", true), Ok(0));
    assert_eq!(
        walk(&mut t, "/mnt/a/./f", true),
        Ok(t.id("mnt/a/f").unwrap())
    );
    assert_eq!(
        walk(&mut t, "//mnt///a//f//", true),
        Ok(t.id("mnt/a/f").unwrap())
    );
    // A file in the middle of a path is ENOTDIR, not ENOENT.
    assert_eq!(walk(&mut t, "/mnt/a/f/x", true), Err(WalkError::NotDir));
    assert_eq!(walk(&mut t, "/nope", true), Err(WalkError::NotFound));
}

#[test]
fn budget_boundary_is_exactly_symloop_max() {
    // A chain of `n` links ending at a file: crossing `n` links must succeed for
    // n == SYMLOOP_MAX and fail for n == SYMLOOP_MAX + 1.
    for n in [SYMLOOP_MAX as usize - 1, SYMLOOP_MAX as usize] {
        let mut t = Tree::new();
        t.make("mnt/target", Kind::File);
        for i in 0..n {
            let next = if i + 1 == n {
                "/target".to_string()
            } else {
                format!("/l{}", i + 1)
            };
            t.make(&format!("mnt/l{}", i), Kind::Link(next));
        }
        let target = t.id("mnt/target").unwrap();
        assert_eq!(
            walk(&mut t, "/mnt/l0", true),
            Ok(target),
            "chain of {n} links"
        );
    }
    let mut t = Tree::new();
    t.make("mnt/target", Kind::File);
    let n = SYMLOOP_MAX as usize + 1;
    for i in 0..n {
        let next = if i + 1 == n {
            "/target".to_string()
        } else {
            format!("/l{}", i + 1)
        };
        t.make(&format!("mnt/l{}", i), Kind::Link(next));
    }
    assert_eq!(walk(&mut t, "/mnt/l0", true), Err(WalkError::TooManyLinks));
}

#[test]
fn expansion_is_capped() {
    let mut t = Tree::new();
    t.make("mnt/a", Kind::Dir);
    let long = format!("/{}", "z".repeat(MAX_EXPANDED_BYTES));
    t.make("mnt/a/big", Kind::Link(long));
    assert_eq!(walk(&mut t, "/mnt/a/big", true), Err(WalkError::TooLong));
}

#[test]
fn guest_target_mapping() {
    // `/x` lives in the ext2 tree mounted at /mnt…
    assert_eq!(
        map_guest_target("/usr/bin/python3"),
        LinkTarget {
            path: "/mnt/usr/bin/python3".to_string(),
            absolute: true
        }
    );
    // …while kernel-owned trees keep the root, and an already-mapped target is
    // not double-prefixed.
    for keep in ["/mnt/a", "/dev/null", "/proc/self/exe", "/tmp/x", "/sys/y"] {
        assert_eq!(
            map_guest_target(keep),
            LinkTarget {
                path: keep.to_string(),
                absolute: true
            }
        );
    }
    // Relative targets stay verbatim and are flagged relative.
    assert_eq!(
        map_guest_target("../real"),
        LinkTarget {
            path: "../real".to_string(),
            absolute: false
        }
    );
    // `..` is *not* collapsed: the walker's stack must see it.
    assert_eq!(map_guest_target("/a/../b").path, "/mnt/a/../b");
}

#[test]
fn components_drops_dots_and_keeps_parent() {
    assert_eq!(components("/a//b/./c/"), vec!["a", "b", "c"]);
    assert_eq!(components("/a/../b"), vec!["a", "..", "b"]);
    assert_eq!(components("/"), Vec::<&str>::new());
    assert_eq!(components(""), Vec::<&str>::new());
}

/// The module's copy of the guest-root policy must never drift from the syscall
/// layer's original (`arch::x86_64::linux::io`).
#[test]
fn link_walk_agrees_with_io_guest_root() {
    for path in [
        "/",
        "/mnt",
        "/mnt/a/b",
        "/dev",
        "/dev/null",
        "/proc",
        "/proc/self/exe",
        "/sys",
        "/tmp",
        "/usr",
        "/usr/bin/x",
        "/process",
        "/procfoo",
        "/devices",
        "/etc/resolv.conf",
    ] {
        assert_eq!(
            guest_path_keeps_root(path),
            crate::io::guest_path_keeps_root(path),
            "policy drift for {path}"
        );
    }
}

// ─────────────── independent oracle over randomized trees ───────────────

/// Restart-based reference resolver, written from the Linux algorithm and
/// deliberately *not* sharing code with the module's in-place splice: on every
/// crossed link it rebuilds the request from the target and restarts the walk.
fn oracle(tree: &Tree, path: &str, follow_final: bool) -> Result<usize, WalkError> {
    let mut req: Vec<String> = components(path).iter().map(|c| c.to_string()).collect();
    let mut expanded = path.len();
    let mut links: u32 = 0;

    'restart: loop {
        let mut resolved: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < req.len() {
            let comp = req[i].clone();
            if comp == ".." {
                resolved.pop();
                i += 1;
                continue;
            }
            let mut here = 0usize;
            for part in &resolved {
                here = match tree.nodes[here].children.get(part) {
                    Some(c) => *c,
                    None => return Err(WalkError::NotFound),
                };
            }
            let child = match tree.nodes[here].children.get(&comp) {
                Some(c) => *c,
                None => return Err(WalkError::NotFound),
            };
            let is_last = i + 1 == req.len();
            if let Kind::Link(target) = &tree.nodes[child].kind {
                if follow_final || !is_last {
                    links += 1;
                    if links > SYMLOOP_MAX {
                        return Err(WalkError::TooManyLinks);
                    }
                    let mapped = map_guest_target(target);
                    if mapped.path.is_empty() {
                        return Err(WalkError::NotFound);
                    }
                    expanded += mapped.path.len();
                    if expanded > MAX_EXPANDED_BYTES {
                        return Err(WalkError::TooLong);
                    }
                    let mut next: Vec<String> = components(&mapped.path)
                        .iter()
                        .map(|c| c.to_string())
                        .collect();
                    next.extend(req[i + 1..].iter().cloned());
                    req = next;
                    if !mapped.absolute {
                        // Relative: keep the already-resolved prefix.
                        let mut merged: Vec<String> = resolved.clone();
                        merged.extend(req.iter().cloned());
                        req = merged;
                    }
                    continue 'restart;
                }
            }
            if !is_last && !matches!(tree.nodes[child].kind, Kind::Dir) {
                return Err(WalkError::NotDir);
            }
            resolved.push(comp);
            i += 1;
        }
        // Fully consumed: walk the resolved components one last time.
        let mut id = 0usize;
        for part in &resolved {
            id = match tree.nodes[id].children.get(part) {
                Some(c) => *c,
                None => return Err(WalkError::NotFound),
            };
        }
        return Ok(id);
    }
}

fn tree_spec() -> impl Strategy<Value = Vec<(String, u8, String)>> {
    let name = "[a-c]{1,3}";
    let target = prop_oneof![
        Just("/x".to_string()),
        Just("/x/y".to_string()),
        Just("/a".to_string()),
        Just("/a/b".to_string()),
        Just("..".to_string()),
        Just("../x".to_string()),
        Just("x/y".to_string()),
        Just("/missing".to_string()),
    ];
    prop::collection::vec((name, 0u8..3, target), 1..12)
}

fn query_path() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just("a".to_string()),
            Just("b".to_string()),
            Just("c".to_string()),
            Just("..".to_string()),
            Just(".".to_string()),
            Just("x".to_string()),
            Just("y".to_string()),
            Just("missing".to_string()),
        ],
        0..6,
    )
    .prop_map(|comps: Vec<String>| format!("/mnt/{}", comps.join("/")))
}

fn build(spec: &[(String, u8, String)]) -> Tree {
    let mut t = Tree::new();
    // The tree models the kernel namespace: ext2 lives at /mnt, so a link target
    // written as `/x/y` (a guest path) maps to `/mnt/x/y`.
    t.make("mnt/x/y", Kind::File); // a shared target pool the links may name
    t.make("mnt/a/b", Kind::Dir);
    for (path, kind, target) in spec {
        if path.is_empty() {
            continue;
        }
        let k = match kind {
            0 => Kind::Dir,
            1 => Kind::File,
            _ => Kind::Link(target.clone()),
        };
        t.make(&format!("mnt/{}", path), k);
    }
    t
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The walker and the independent restart-based oracle agree on every
    /// randomized tree/path pair — same node, or the same `WalkError`.
    #[test]
    fn walker_matches_the_independent_oracle(
        spec in tree_spec(),
        path in query_path(),
        follow in any::<bool>(),
    ) {
        let mut tree = build(&spec);
        let got = resolve(&mut tree, &path, follow);
        let want = oracle(&tree, &path, follow);
        prop_assert_eq!(got, want, "path {} follow {}", path, follow);
    }

    /// Resolution always terminates, never panics, and a successful result is a
    /// node the tree really contains.
    #[test]
    fn walker_is_total(spec in tree_spec(), path in query_path(), follow in any::<bool>()) {
        let mut tree = build(&spec);
        if let Ok(id) = resolve(&mut tree, &path, follow) {
            prop_assert!(id < tree.nodes.len());
        }
    }
}
