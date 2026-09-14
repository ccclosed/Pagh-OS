//! When may pagh format a block device?
//!
//! `boot::init_fs` used to format whenever `Ext2Fs::mount` failed *and* the ext2
//! superblock magic was absent. On a real machine that is the normal case for the
//! machine's **own** disk: the check reads byte 1024 of the raw namespace, which on
//! a GPT/MBR disk is the partition entry array, so a partitioned non-ext2 disk was
//! classified "blank" and erased — partition table, per-group metadata across the
//! whole device and the backup GPT at the tail (issue #33).
//!
//! The policy below is the fix. It is deliberately **pure** (`core`-only, no I/O):
//! the caller probes the device and feeds the bytes in, so `host-tests` can assert
//! the decision itself against real GPT/MBR/ext2 boot areas (property P50) instead
//! of hoping a QEMU run happens to cover it — no test in CI ever boots the kernel.
//!
//! Rule, in order:
//!
//! 1. a *mountable* ext2 superblock means "this is a filesystem" — never format,
//!    whatever else the device looks like (`Refusal::ExistingFilesystem`);
//! 2. a device whose probed boot area is entirely zero is genuinely blank and is
//!    the only thing pagh formats on its own;
//! 3. `allow_destructive` (an explicit opt-in, not wired to a caller yet) is the
//!    only way past a foreign layout;
//! 4. everything else is refused, naming what was found.

/// Byte offset of the ext2 superblock inside the first filesystem block.
pub const SUPERBLOCK_OFFSET: usize = 1024;

/// ext2/ext4 `s_magic` as it appears on disk (little-endian `0xEF53`).
pub const EXT2_MAGIC_LE: [u8; 2] = [0x53, 0xEF];

/// GPT header signature, at LBA 1 (byte 512).
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// MBR boot signature, at the end of LBA 0 (byte 510).
pub const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// Minimum probe length that can recognise every layout above (the ext2 magic).
pub const MIN_PROBE: usize = SUPERBLOCK_OFFSET + 2;

/// Explicit opt-in marker an operator writes into sector 0 to authorise pagh to
/// take over a device that already carries data.
///
/// ```text
/// printf 'PAGH-FORMAT' | sudo dd of=/dev/nvme0n1 bs=1 conv=notrunc
/// ```
///
/// Deliberate and unmistakable: nothing else in the kernel or in any filesystem
/// writes these bytes, the write itself *is* the operator saying "this device may
/// be destroyed", and it cannot happen by accident — unlike a bootloader command
/// line, which is easy to add once and forget. Without the marker, a device is
/// formatted only when it is entirely zero.
pub const OPT_IN_MARKER: &[u8] = b"PAGH-FORMAT";

/// What the probed boot area of a device looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The ext2/ext4 superblock magic is present.
    Ext2,
    /// An MBR or GPT partition table is present.
    PartitionTable,
    /// Every probed byte is zero.
    Blank,
    /// Non-zero data that is neither of the above.
    Foreign,
}

impl Layout {
    /// Short name for the boot log line.
    pub fn name(self) -> &'static str {
        match self {
            Layout::Ext2 => "ext2-like",
            Layout::PartitionTable => "partitioned",
            Layout::Blank => "blank",
            Layout::Foreign => "foreign data",
        }
    }
}

/// Why a device must not be formatted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A valid ext2 superblock is present: a real filesystem that failed to mount.
    ExistingFilesystem,
    /// The device carries a partition table (MBR or GPT).
    PartitionTable,
    /// The ext2 magic is present but the superblock does not validate.
    Ext2Like,
    /// Non-zero bytes that are neither a partition table nor an ext2 superblock.
    ForeignData,
}

impl Refusal {
    /// One-line explanation for the serial log.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::ExistingFilesystem => {
                "existing ext2 mount failed; refusing destructive reformat"
            }
            Refusal::PartitionTable => "device carries an MBR/GPT partition table",
            Refusal::Ext2Like => "device looks like ext2 but its superblock does not validate",
            Refusal::ForeignData => "device is neither blank nor ext2 (unknown on-disk data)",
        }
    }
}

/// Incremental boot-area classifier.
///
/// Construct with the first filesystem block (≥ [`MIN_PROBE`] bytes, 4 KiB in
/// practice) and `feed` further chunks while [`Probe::is_decided`] is false. The
/// caller owns the I/O and may stop as soon as the layout is decided; a device
/// only has to be scanned to the end when it is claiming to be blank.
///
/// Fail-closed by construction: a probe shorter than [`MIN_PROBE`] is
/// [`Layout::Foreign`] (refused), never `Blank`.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    layout: Layout,
    /// Still `Blank`-so-far: a later non-zero byte turns it into `Foreign`.
    provisional_blank: bool,
    /// The operator's [`OPT_IN_MARKER`] was found in sector 0.
    marked: bool,
}

impl Probe {
    /// Classify the first filesystem block.
    pub fn new(first_block: &[u8]) -> Self {
        let marked = first_block.len() >= OPT_IN_MARKER.len()
            && &first_block[..OPT_IN_MARKER.len()] == OPT_IN_MARKER;
        if first_block.len() < MIN_PROBE {
            return Self {
                layout: Layout::Foreign,
                provisional_blank: false,
                marked,
            };
        }
        let layout = if is_ext2_superblock(first_block) {
            Layout::Ext2
        } else if is_partition_table(first_block) {
            Layout::PartitionTable
        } else if is_zero(first_block) {
            Layout::Blank
        } else {
            Layout::Foreign
        };
        Self {
            layout,
            provisional_blank: layout == Layout::Blank,
            marked,
        }
    }

    /// A probe that could not be taken (I/O error). Refused, never formatted.
    pub fn unreadable() -> Self {
        Self {
            layout: Layout::Foreign,
            provisional_blank: false,
            marked: false,
        }
    }

    /// Feed the next chunk of the boot area.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.provisional_blank && !is_zero(chunk) {
            self.layout = Layout::Foreign;
            self.provisional_blank = false;
        }
    }

    /// The layout decided so far.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Was the explicit [`OPT_IN_MARKER`] found? This is the value to pass as
    /// `allow_destructive`.
    pub fn is_marked(&self) -> bool {
        self.marked
    }

    /// `true` once the layout can no longer change (no further probe is needed).
    pub fn is_decided(&self) -> bool {
        !self.provisional_blank
    }
}

/// The one decision `boot::init_fs` makes: may this device be formatted?
///
/// `has_valid_superblock` must be the result of `Ext2Fs::has_valid_superblock`
/// (a *parsable* superblock, not merely the magic), and `layout` the result of
/// probing the device's boot area.
pub fn format_allowed(
    layout: Layout,
    has_valid_superblock: bool,
    allow_destructive: bool,
) -> Result<(), Refusal> {
    if has_valid_superblock {
        // A real filesystem that failed to mount is an error, never permission
        // to erase user data. This is the pre-existing guard, kept first.
        return Err(Refusal::ExistingFilesystem);
    }
    if layout == Layout::Blank {
        return Ok(());
    }
    if allow_destructive {
        return Ok(());
    }
    Err(match layout {
        Layout::PartitionTable => Refusal::PartitionTable,
        Layout::Ext2 => Refusal::Ext2Like,
        // `Blank` returned above; listed for exhaustiveness.
        Layout::Blank | Layout::Foreign => Refusal::ForeignData,
    })
}

/// ext2/ext4 superblock magic at [`SUPERBLOCK_OFFSET`].
fn is_ext2_superblock(buf: &[u8]) -> bool {
    buf.len() >= MIN_PROBE && buf[SUPERBLOCK_OFFSET..MIN_PROBE] == EXT2_MAGIC_LE
}

/// GPT header at LBA 1 or the MBR boot signature at the end of LBA 0.
///
/// A protective MBR (GPT) carries both, so checking either alone is enough to
/// refuse; both are checked so a hybrid or MBR-only layout is caught too.
fn is_partition_table(buf: &[u8]) -> bool {
    let gpt = buf.len() >= 520 && &buf[512..520] == GPT_SIGNATURE;
    let mbr = buf.len() >= 512 && buf[510..512] == MBR_SIGNATURE;
    gpt || mbr
}

/// No non-zero byte anywhere in the slice (empty slices are trivially zero).
fn is_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt_boot_block() -> [u8; 4096] {
        let mut b = [0u8; 4096];
        // Protective MBR: one 0xEE partition entry, then the boot signature.
        b[446] = 0x00; // not bootable
        b[450] = 0xEE; // type: GPT protective
        b[454] = 0x01; // first LBA 1 (little-endian u32)
        b[510] = 0x55;
        b[511] = 0xAA;
        b[512..520].copy_from_slice(GPT_SIGNATURE);
        b
    }

    fn ext2_boot_block() -> [u8; 4096] {
        let mut b = [0u8; 4096];
        b[SUPERBLOCK_OFFSET..MIN_PROBE].copy_from_slice(&EXT2_MAGIC_LE);
        b
    }

    #[test]
    fn blank_probe_is_blank_and_decided_only_after_the_scan() {
        let mut p = Probe::new(&[0u8; 4096]);
        assert_eq!(p.layout(), Layout::Blank);
        assert!(
            !p.is_decided(),
            "blank stays provisional while chunks remain"
        );
        p.feed(&[0u8; 4096]);
        assert_eq!(p.layout(), Layout::Blank);
    }

    #[test]
    fn later_non_zero_chunk_makes_a_blank_probe_foreign() {
        let mut p = Probe::new(&[0u8; 4096]);
        p.feed(&[0u8; 4096]);
        let mut dirty = [0u8; 4096];
        dirty[4095] = 1;
        p.feed(&dirty);
        assert_eq!(p.layout(), Layout::Foreign);
        assert!(p.is_decided());
    }

    #[test]
    fn layouts_are_recognised() {
        assert_eq!(
            Probe::new(&gpt_boot_block()).layout(),
            Layout::PartitionTable
        );
        assert_eq!(Probe::new(&ext2_boot_block()).layout(), Layout::Ext2);
        assert_eq!(Probe::new(&[0u8; 4096]).layout(), Layout::Blank);
        let mut junk = [0u8; 4096];
        junk[0] = 0xEB; // x86 boot sector jump, e.g. NTFS/XFS/LUKS
        assert_eq!(Probe::new(&junk).layout(), Layout::Foreign);
    }

    #[test]
    fn short_probe_fails_closed() {
        let p = Probe::new(&[0u8; 16]);
        assert_eq!(p.layout(), Layout::Foreign);
        assert!(p.is_decided());
        assert_eq!(Probe::unreadable().layout(), Layout::Foreign);
    }

    #[test]
    fn only_a_blank_device_is_formatted_by_default() {
        assert_eq!(format_allowed(Layout::Blank, false, false), Ok(()));
        assert_eq!(
            format_allowed(Layout::PartitionTable, false, false),
            Err(Refusal::PartitionTable)
        );
        assert_eq!(
            format_allowed(Layout::Ext2, false, false),
            Err(Refusal::Ext2Like)
        );
        assert_eq!(
            format_allowed(Layout::Foreign, false, false),
            Err(Refusal::ForeignData)
        );
        assert_eq!(
            format_allowed(Layout::Foreign, false, true),
            Ok(()),
            "an explicit opt-in is the only way past a foreign layout"
        );
    }

    #[test]
    fn a_mountable_superblock_is_never_formatted() {
        for layout in [
            Layout::Blank,
            Layout::Ext2,
            Layout::PartitionTable,
            Layout::Foreign,
        ] {
            for allow in [false, true] {
                assert_eq!(
                    format_allowed(layout, true, allow),
                    Err(Refusal::ExistingFilesystem),
                    "layout {layout:?} allow={allow}"
                );
            }
        }
    }

    #[test]
    fn the_opt_in_marker_is_recognised_only_where_written() {
        let mut marked = gpt_boot_block();
        marked[..OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
        let p = Probe::new(&marked);
        assert!(p.is_marked());
        // The marker authorises exactly one layout class, and the partition
        // table underneath is still reported.
        assert_eq!(p.layout(), Layout::PartitionTable);
        assert_eq!(format_allowed(p.layout(), false, p.is_marked()), Ok(()));

        assert!(!Probe::new(&gpt_boot_block()).is_marked());
        assert!(!Probe::new(&[0u8; 4096]).is_marked());
        assert!(!Probe::unreadable().is_marked());

        // A marker later in the window is not a marker: it must be in sector 0.
        let mut late = [0u8; 4096];
        late[2048..2048 + OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
        assert!(!Probe::new(&late).is_marked());
        assert_eq!(Probe::new(&late).layout(), Layout::Foreign);
    }

    #[test]
    fn the_marker_does_not_override_a_mountable_filesystem() {
        let mut marked = ext2_boot_block();
        marked[..OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
        let p = Probe::new(&marked);
        assert!(p.is_marked());
        assert_eq!(
            format_allowed(p.layout(), true, p.is_marked()),
            Err(Refusal::ExistingFilesystem)
        );
    }
}
