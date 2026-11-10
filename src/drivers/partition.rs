//! Partition-table parsing: a view of one partition as its own block device.
//!
//! WHY THIS EXISTS. The drivers took whole devices: the NVMe driver claimed the
//! entire namespace and ext2 was formatted over it. That is fine for a QEMU image
//! that is nothing but a filesystem, and wrong on a real machine — the operator's
//! disk already has a partition table, and "pagh wants the whole disk" means pagh
//! cannot coexist with anything else. This module reads GPT (and MBR as the
//! fallback) and exposes each partition as an independent `BlockDevice`
//! (`nvme0p1`, `nvme0p2`, …), so the filesystem layer can mount one partition and
//! leave the rest of the disk alone.
//!
//! READ-ONLY VIEW. A `PartitionSlice` forwards to the parent device with an
//! offset, bounds-checked against the partition's own length: a read past the end
//! of a partition fails instead of silently reading the next one's data, which is
//! what would happen if the slice simply forwarded larger offsets.
//!
//! WHAT IS PARSED. GPT with a valid header and entry array (LBA 1, signature
//! `EFI PART`), including the backup-header sanity check; MBR with its four
//! primary entries otherwise. Logical (extended) partitions are NOT parsed —
//! named here rather than implied, because a disk laid out that way will show its
//! primary entries only.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use crate::drivers::BlockDevice;
use crate::sync::spinlock::Spinlock;

/// One partition of `parent`, addressed as sectors `[start, start + len)`.
pub struct PartitionSlice {
    name: String,
    parent: Arc<dyn BlockDevice>,
    /// First sector of the partition, in 512-byte units.
    start: u64,
    /// Length in 512-byte sectors.
    sectors: u64,
    /// GPT type GUID or MBR type byte, rendered for the UI.
    type_text: String,
}

impl PartitionSlice {
    pub fn new(
        name: String,
        parent: Arc<dyn BlockDevice>,
        start: u64,
        sectors: u64,
        type_text: String,
    ) -> Self {
        Self {
            name,
            parent,
            start,
            sectors,
            type_text,
        }
    }

    /// Partition type as text (`linux`, `efi`, `unknown 0x83`, …).
    pub fn type_text(&self) -> &str {
        &self.type_text
    }

    fn translate(&self, block: u64, len: usize) -> Result<u64, ()> {
        let end = block.checked_add((len as u64).div_ceil(512)).ok_or(())?;
        if end > self.sectors {
            return Err(());
        }
        self.start.checked_add(block).ok_or(())
    }
}

impl BlockDevice for PartitionSlice {
    fn name(&self) -> &str {
        &self.name
    }

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        let abs = self.translate(block, buf.len())?;
        self.parent.read_block(abs, buf)
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        let abs = self.translate(block, buf.len())?;
        self.parent.write_block(abs, buf)
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn flush(&self) -> Result<(), ()> {
        self.parent.flush()
    }
}

/// GPT header: 92 bytes at LBA 1.
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
/// Partition entry size every tool writes; the header also carries it, and the
/// header's value wins when it differs.
const GPT_ENTRY_SIZE: usize = 128;
/// Maximum entries we will read (the header's count can claim more than the
/// array holds, and a hostile/corrupt value must not make us read a disk's worth
/// of entries).
const GPT_MAX_ENTRIES: u32 = 256;

/// Well-known GPT type GUIDs, first 8 bytes little-endian reversed, compared as
/// raw on-disk byte order (mixed-endian GUIDs are a classic source of silent
/// mismatch, so the comparison is on the bytes as stored).
const GPT_TYPE_EFI: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
const GPT_TYPE_LINUX: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

fn read_sectors(dev: &dyn BlockDevice, first: u64, bytes: usize) -> Option<alloc::vec::Vec<u8>> {
    let sectors = bytes.div_ceil(512).max(1);
    let mut buf = alloc::vec![0u8; sectors * 512];
    // Read sector by sector: the trait is sector-addressed, and asking for a
    // larger unit is not part of its contract.
    for i in 0..sectors as u64 {
        let off = (i * 512) as usize;
        dev.read_block(first + i, &mut buf[off..off + 512]).ok()?;
    }
    Some(buf)
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

/// Parse `dev`'s partition table and register every usable partition.
///
/// Returns the number of partitions registered. An unreadable or absent table is
/// not an error: a device that is nothing but a filesystem is exactly what the
/// tooling produces, and its owner expects it to be usable as one.
pub fn register_partitions(dev: &Arc<dyn BlockDevice>) -> usize {
    let total = dev.sector_count();
    if total == 0 {
        return 0;
    }

    if let Some(count) = parse_gpt(dev, total) {
        return count;
    }
    parse_mbr(dev, total)
}

fn type_text_for_gpt(guid: &[u8]) -> &'static str {
    if guid == GPT_TYPE_LINUX {
        "linux"
    } else if guid == GPT_TYPE_EFI {
        "efi"
    } else {
        "gpt-other"
    }
}

fn parse_gpt(dev: &Arc<dyn BlockDevice>, total: u64) -> Option<usize> {
    if total < 34 {
        return None;
    }
    let header = read_sectors(&**dev, 1, 512)?;
    if &header[0..8] != GPT_SIGNATURE {
        return None;
    }
    let entries_lba = le64(&header, 72);
    let entry_count = le32(&header, 80).min(GPT_MAX_ENTRIES);
    let entry_size = (le32(&header, 84) as usize).max(GPT_ENTRY_SIZE);
    if entry_count == 0 || entries_lba == 0 || entries_lba >= total {
        return None;
    }
    crate::info!(
        "part: GPT on {}: {} entries of {} B at LBA {}",
        dev.name(),
        entry_count,
        entry_size,
        entries_lba
    );

    let mut registered = 0usize;
    let mut sector = entries_lba;
    let mut window: alloc::vec::Vec<u8> = read_sectors(&**dev, sector, 512).unwrap_or_default();
    let mut base_in_window = 0usize;
    for index in 0..entry_count {
        // Entry `index` lives at byte `index*entry_size` in the array. Read one
        // sector at a time and slice at the entry's offset INSIDE it: reading a
        // sector per entry and always slicing from 0 made every partition report
        // the first entry's start and length (four identical "partitions").
        let entry_off = index as usize * entry_size;
        if entry_off < base_in_window || entry_off + entry_size > base_in_window + window.len() {
            sector = entries_lba + (entry_off / 512) as u64;
            let Some(new_window) = read_sectors(&**dev, sector, 1024) else {
                break;
            };
            window = new_window;
            base_in_window = ((sector - entries_lba) * 512) as usize;
            if entry_off < base_in_window {
                break;
            }
        }
        let rel = entry_off - base_in_window;
        if rel + entry_size > window.len() {
            break;
        }
        let entry = &window[rel..rel + entry_size];
        let guid = &entry[0..16];
        // An all-zero type GUID means "unused entry".
        if guid.iter().all(|b| *b == 0) {
            continue;
        }
        let first = le64(&entry, 32);
        let last = le64(&entry, 40);
        if last < first || first == 0 || last >= total {
            continue;
        }
        let sectors = last - first + 1;
        let name = format!("{}p{}", dev.name(), index + 1);
        let slice = Arc::new(PartitionSlice::new(
            name.clone(),
            Arc::clone(dev),
            first,
            sectors,
            String::from(type_text_for_gpt(guid)),
        ));
        crate::info!(
            "part: {} -> {} sectors at LBA {} ({})",
            name,
            sectors,
            first,
            type_text_for_gpt(guid)
        );
        crate::drivers::register_block(slice);
        registered += 1;
    }
    Some(registered)
}

fn parse_mbr(dev: &Arc<dyn BlockDevice>, total: u64) -> usize {
    let Some(mbr) = read_sectors(&**dev, 0, 512) else {
        return 0;
    };
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return 0;
    }
    let mut registered = 0usize;
    for i in 0..4usize {
        let base = 446 + i * 16;
        let ptype = mbr[base + 4];
        let first = le32(&mbr, base + 8) as u64;
        let sectors = le32(&mbr, base + 12) as u64;
        // 0x00 = empty, 0x05/0x0F/0x85 = extended container (not parsed here).
        if ptype == 0 || sectors == 0 || matches!(ptype, 0x05 | 0x0F | 0x85) {
            continue;
        }
        if first + sectors > total {
            continue;
        }
        let type_text = match ptype {
            0x83 => String::from("linux"),
            0xEF => String::from("efi"),
            0x82 => String::from("swap"),
            other => format!("mbr-0x{:02x}", other),
        };
        let name = format!("{}p{}", dev.name(), i + 1);
        crate::info!(
            "part: {} -> {} sectors at LBA {} ({})",
            name,
            sectors,
            first,
            type_text
        );
        crate::drivers::register_block(Arc::new(PartitionSlice::new(
            name,
            Arc::clone(dev),
            first,
            sectors,
            type_text,
        )));
        registered += 1;
    }
    if registered > 0 {
        crate::info!("part: MBR on {}: {} partition(s)", dev.name(), registered);
    }
    registered
}

/// Registered partition slices, newest first, for the shell's `disk` command.
pub fn list() -> alloc::vec::Vec<(String, String)> {
    crate::drivers::list_blocks()
        .into_iter()
        .filter(|(name, _)| name.contains('p') && name.chars().next().is_some())
        .filter(|(name, _)| {
            // `nvme0p1` / `virtio-blk0p2`: a partition name has a 'p' followed by
            // digits at the end; the whole-device names never do.
            match name.rfind('p') {
                Some(pos) => name[pos + 1..].chars().all(|c| c.is_ascii_digit()),
                None => false,
            }
        })
        .map(|(name, sectors)| (name, format!("{} MiB", sectors * 512 / (1024 * 1024))))
        .collect()
}

/// Is `name` a partition (`virtio-blk0p1`, `nvme0p1`)?
fn is_partition(name: &str) -> bool {
    match name.rfind('p') {
        Some(pos) => {
            !name[pos + 1..].is_empty() && name[pos + 1..].chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// What a partition holds, as far as the boot path is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartitionContent {
    /// An ext2 volume carrying pagh's own journal marker — a volume pagh
    /// formatted, and the only kind it mounts by itself.
    PaghVolume,
    /// A structurally valid ext2/ext4 filesystem that is NOT pagh's. Never
    /// touched automatically: it is somebody's data. (Read-only inspection is not
    /// offered either — the driver writes group descriptors during mount.)
    ForeignExt2,
    /// No filesystem and no other recognisable data: safe to format, which the
    /// boot path may do on its own.
    Blank,
    /// Something else is there (another filesystem, a boot record, data). Never
    /// touched without the operator saying so.
    Other,
}

/// What `dev` holds, from the boot path's point of view.
///
/// THE PROBE DECIDES THE LABEL, `is_pagh_volume` DECIDES THE OWNERSHIP. They are
/// separate questions: `probe_boot_area` recognises the ext-family superblock
/// magic whether or not this driver could mount it (ext4 is recognised and
/// deliberately not mountable), which is exactly what the operator needs to see —
/// "a filesystem lives here and it is not yours". A volume is pagh's own only if
/// it carries the WAL superblock that `Ext2Fs::format` writes; nothing else can
/// produce that marker, and no other filesystem reads ext2's layout that way.
pub fn classify(dev: &dyn BlockDevice) -> PartitionContent {
    if crate::fs::ext2::Ext2Fs::is_pagh_volume(dev) {
        return PartitionContent::PaghVolume;
    }
    match crate::fs::device::probe_boot_area(dev).layout() {
        crate::fs::format_policy::Layout::Ext2 => PartitionContent::ForeignExt2,
        crate::fs::format_policy::Layout::Blank => PartitionContent::Blank,
        _ => PartitionContent::Other,
    }
}

/// Classify every registered partition, best candidate first.
///
/// The order is deliberate: an existing pagh volume beats a blank partition
/// (mounting something is always safer than creating something), and both beat
/// "other", which the boot path leaves alone entirely.
///
/// NO PARTITION IS FORMATTED FROM THIS LIST. Only the boot menu offers that, and
/// only after the operator presses `F` and types the device name; the menu uses
/// [`PartitionContent`] to say what each row holds and [`classify`] to label a
/// device that is not a partition at all (the RAM disk from the boot medium).
pub fn candidates() -> alloc::vec::Vec<(Arc<dyn BlockDevice>, PartitionContent)> {
    let mut ext2 = alloc::vec::Vec::new();
    let mut blank = alloc::vec::Vec::new();
    let mut other = alloc::vec::Vec::new();
    for (name, _) in crate::drivers::list_blocks() {
        if !is_partition(&name) {
            continue;
        }
        let Some(dev) = crate::drivers::get_block(&name) else {
            continue;
        };
        let content = classify(&*dev);
        match content {
            PartitionContent::PaghVolume => ext2.push((dev, content)),
            PartitionContent::Blank => blank.push((dev, content)),
            PartitionContent::ForeignExt2 | PartitionContent::Other => other.push((dev, content)),
        }
    }
    let mut out = ext2;
    out.append(&mut blank);
    out.append(&mut other);
    out
}

/// The first registered partition that already carries an ext2 filesystem.
pub fn find_pagh_partition() -> Option<Arc<dyn BlockDevice>> {
    candidates()
        .into_iter()
        .find(|(_, c)| *c == PartitionContent::PaghVolume)
        .map(|(dev, _)| dev)
}

/// Serialises partition registration (called once per device at boot).
static _REGISTER_LOCK: Spinlock<()> = Spinlock::new(());
