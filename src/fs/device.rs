//! Device-level helpers shared by the boot path and the shell's `disk` menu.
//!
//! The *decision* "may this device be formatted?" is pure and lives in
//! [`crate::fs::format_policy`], which host-tests include directly. Reading the
//! device to feed that decision is I/O, so it lives here — one implementation
//! used by both callers, because a menu that classified a device differently
//! from the boot path would offer to erase exactly the disks the boot path
//! refuses to touch.

/// Highest byte offset the probe inspects.
///
/// One MiB is the smallest window that contains everything the classifier has to
/// recognise: the ext2 superblock at 1024, an MBR at the end of sector 0 and a
/// GPT header with its partition entry array at LBA 1..33.
const PROBE_LIMIT: u64 = 1024 * 1024;

/// Probe granularity, in bytes: one filesystem block (8 device sectors).
const PROBE_CHUNK: u64 = 4096;

/// Classify the first MiB of `dev` for [`crate::fs::format_policy`].
///
/// Reads in [`PROBE_CHUNK`] steps and stops as soon as the layout is decided — a
/// partitioned or populated device costs a single read; only a device claiming to
/// be blank is scanned to the end of [`PROBE_LIMIT`]. A read error keeps the
/// `Foreign` (refused) classification, so an unreadable device is never formatted
/// either — including a device with a single bad sector inside the window, which
/// therefore cannot be formatted at all until it is zeroed.
pub fn probe_boot_area(dev: &dyn crate::drivers::BlockDevice) -> crate::fs::format_policy::Probe {
    use crate::fs::format_policy::Probe;

    const SECTORS_PER_CHUNK: u64 = PROBE_CHUNK / 512;
    let mut buf = alloc::vec![0u8; PROBE_CHUNK as usize];
    let mut probe = match dev.read_block(0, &mut buf) {
        Ok(_) => Probe::new(&buf),
        Err(()) => return Probe::unreadable(),
    };
    let mut sector = SECTORS_PER_CHUNK;
    while !probe.is_decided() && sector * 512 < PROBE_LIMIT {
        match dev.read_block(sector, &mut buf) {
            Ok(_) => probe.feed(&buf),
            Err(()) => return Probe::unreadable(),
        }
        sector += SECTORS_PER_CHUNK;
    }
    probe
}
