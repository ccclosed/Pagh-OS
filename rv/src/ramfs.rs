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

const SECTOR: usize = 512;
/// Base sector for the persisted image (a high, reserved region; the low sectors
/// are left for a future real filesystem).
const BASE_SECTOR: usize = 2048;
/// On-disk header magic for the persisted ramfs image ("RVFS").
const MAGIC: [u8; 4] = [b'R', b'V', b'F', b'S'];

/// Serialize the ramfs to a length-prefixed byte image:
/// repeated [u32 name_len][name][u32 content_len][content].
fn serialize() -> Vec<u8> {
    let mut img = Vec::new();
    for (name, _) in list() {
        let content = read(&name).unwrap_or_default();
        img.extend_from_slice(&(name.len() as u32).to_le_bytes());
        img.extend_from_slice(name.as_bytes());
        img.extend_from_slice(&(content.len() as u32).to_le_bytes());
        img.extend_from_slice(content.as_bytes());
    }
    img
}

/// Persist the ramfs to virtio-blk: sector 0 = header (magic + image length),
/// sectors 1.. = the serialized image. Returns the number of files saved.
pub fn save() -> Option<usize> {
    if crate::blk::capacity().is_none() {
        return None;
    }
    let img = serialize();
    let count = list().len();

    let mut hdr = [0u8; SECTOR];
    hdr[0..4].copy_from_slice(&MAGIC);
    hdr[4..8].copy_from_slice(&(img.len() as u32).to_le_bytes());
    if !crate::blk::write_sector(BASE_SECTOR, &hdr) {
        return None;
    }

    let sectors = img.len().div_ceil(SECTOR);
    for s in 0..sectors {
        let mut buf = [0u8; SECTOR];
        let off = s * SECTOR;
        let n = core::cmp::min(SECTOR, img.len() - off);
        buf[..n].copy_from_slice(&img[off..off + n]);
        if !crate::blk::write_sector(BASE_SECTOR + 1 + s, &buf) {
            return None;
        }
    }
    Some(count)
}

/// Restore the ramfs from virtio-blk (see [`save`]). Returns the number of files
/// loaded, or `None` if there is no disk or no saved image.
pub fn load() -> Option<usize> {
    if crate::blk::capacity().is_none() {
        return None;
    }
    let mut hdr = [0u8; SECTOR];
    if !crate::blk::read_sector(BASE_SECTOR, &mut hdr) || hdr[0..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;

    let sectors = len.div_ceil(SECTOR);
    let mut img = Vec::with_capacity(sectors * SECTOR);
    for s in 0..sectors {
        let mut buf = [0u8; SECTOR];
        if !crate::blk::read_sector(BASE_SECTOR + 1 + s, &mut buf) {
            return None;
        }
        img.extend_from_slice(&buf);
    }
    img.truncate(len);

    // Parse and populate.
    let mut count = 0;
    let mut i = 0;
    while i + 4 <= img.len() {
        let nl = u32::from_le_bytes([img[i], img[i + 1], img[i + 2], img[i + 3]]) as usize;
        i += 4;
        if i + nl + 4 > img.len() {
            break;
        }
        let name = String::from_utf8_lossy(&img[i..i + nl]).into_owned();
        i += nl;
        let cl = u32::from_le_bytes([img[i], img[i + 1], img[i + 2], img[i + 3]]) as usize;
        i += 4;
        if i + cl > img.len() {
            break;
        }
        let content = String::from_utf8_lossy(&img[i..i + cl]).into_owned();
        i += cl;
        FS.lock().insert(name, content);
        count += 1;
    }
    Some(count)
}

/// Boot-time persistence self-test: save the current ramfs, clear it, reload it
/// from disk, and report. No input required.
pub fn persist_selftest() {
    write("persist.txt", "survives a save/load round-trip");
    let saved = save();
    // Clear the ramfs, then reload from disk.
    let names: Vec<String> = list().into_iter().map(|(n, _)| n).collect();
    for n in &names {
        remove(n);
    }
    let loaded = load();
    let ok = read("persist.txt").as_deref() == Some("survives a save/load round-trip");
    crate::kprintln!(
        "rv: ramfs persist self-test -- saved {:?}, reloaded {:?}, content OK={}",
        saved,
        loaded,
        ok
    );
    // Leave the ramfs empty for the interactive shell.
    let names: Vec<String> = list().into_iter().map(|(n, _)| n).collect();
    for n in &names {
        remove(n);
    }
}
