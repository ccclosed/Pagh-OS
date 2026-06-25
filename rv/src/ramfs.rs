//! A tiny in-RAM filesystem: a name -> contents map behind a spinlock. Backs the
//! shell's `ls`/`cat`/`write`/`rm` commands. It exercises the heap/`alloc` types
//! and gives the seed a usable (if volatile) file abstraction before the real
//! ext2-over-virtio-blk layer is integrated.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

static FS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// Create or overwrite `name` with `content`.
pub fn write(name: &str, content: &str) {
    FS.lock().insert(name.to_string(), content.to_string());
}

/// Read `name`'s contents, if present.
pub fn read(name: &str) -> Option<String> {
    FS.lock().get(name).cloned()
}

/// Remove `name`; returns whether it existed.
pub fn remove(name: &str) -> bool {
    FS.lock().remove(name).is_some()
}

/// List `(name, size)` pairs, sorted by name.
pub fn list() -> Vec<(String, usize)> {
    FS.lock()
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect()
}

/// Boot-time self-test of the ramfs (no input needed): write two files, read
/// one back, remove it, and report counts. Leaves the ramfs empty for the shell.
pub fn selftest() {
    write("a.txt", "alpha");
    write("b.txt", "beta");
    let before = list().len();
    let a = read("a.txt").unwrap_or_default();
    let removed = remove("a.txt");
    let after = list().len();
    crate::kprintln!(
        "rv: ramfs self-test -- {} files, a.txt='{}', rm={}, now {} files",
        before,
        a,
        removed,
        after
    );
    remove("b.txt");
}
