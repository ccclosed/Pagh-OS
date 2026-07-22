//! `struct stat` encoding for the x86_64 binary-compatibility layer.
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (R11.6). `fstat`/`newfstatat` populate a [`LinuxStat`] from a file's size and
//! mode and copy it into the user buffer; the encoding here is the pure core those
//! handlers reuse (R2.8).
#![allow(dead_code)]

/// File-type bits for a regular file (`S_IFREG`).
///
/// Combined with permission bits to form the `st_mode` value a Linux binary
/// expects (R2.8).
pub const S_IFREG: u32 = 0o100000;

/// File-type bits for a directory (`S_IFDIR`).
pub const S_IFDIR: u32 = 0o040000;

/// A sane default block size reported in `st_blksize`.
pub const DEFAULT_BLKSIZE: i64 = 4096;

/// The x86_64 Linux `struct stat` layout (subset populated).
///
/// Field order, sizes, and padding match the architectural layout exactly so a
/// byte-for-byte copy into the user buffer is interpreted correctly by a Linux
/// binary. `fstat`/`newfstatat` populate `st_size`, `st_mode` (type bits | perms),
/// and `st_blksize`; the remaining fields are zeroed (R2.8).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinuxStat {
    /// Device id containing the file.
    pub st_dev: u64,
    /// Inode number.
    pub st_ino: u64,
    /// Number of hard links.
    pub st_nlink: u64,
    /// File type and mode (e.g. `S_IFREG | 0o644`).
    pub st_mode: u32,
    /// Owning user id.
    pub st_uid: u32,
    /// Owning group id.
    pub st_gid: u32,
    /// Padding to align `st_rdev`.
    pub __pad0: u32,
    /// Device id (if the file is a special file).
    pub st_rdev: u64,
    /// Total size in bytes.
    pub st_size: i64,
    /// Preferred block size for filesystem I/O.
    pub st_blksize: i64,
    /// Number of 512-byte blocks allocated.
    pub st_blocks: i64,
    /// Time of last access (seconds).
    pub st_atime: i64,
    /// Time of last access (nanoseconds).
    pub st_atime_nsec: i64,
    /// Time of last modification (seconds).
    pub st_mtime: i64,
    /// Time of last modification (nanoseconds).
    pub st_mtime_nsec: i64,
    /// Time of last status change (seconds).
    pub st_ctime: i64,
    /// Time of last status change (nanoseconds).
    pub st_ctime_nsec: i64,
    /// Reserved, unused fields.
    pub __unused: [i64; 3],
}

impl LinuxStat {
    /// An all-zero [`LinuxStat`]. Internal seed for [`encode_stat`].
    const fn zeroed() -> Self {
        LinuxStat {
            st_dev: 0,
            st_ino: 0,
            st_nlink: 0,
            st_mode: 0,
            st_uid: 0,
            st_gid: 0,
            __pad0: 0,
            st_rdev: 0,
            st_size: 0,
            st_blksize: 0,
            st_blocks: 0,
            st_atime: 0,
            st_atime_nsec: 0,
            st_mtime: 0,
            st_mtime_nsec: 0,
            st_ctime: 0,
            st_ctime_nsec: 0,
            __unused: [0; 3],
        }
    }

    /// Read back the encoded size from its architectural offset (`st_size`).
    ///
    /// Pure accessor used by Property P8 to recover the input. (R2.8)
    pub fn stat_size(&self) -> i64 {
        self.st_size
    }

    /// Read back the encoded mode from its architectural offset (`st_mode`).
    ///
    /// Pure accessor used by Property P8 to recover the input. (R2.8)
    pub fn stat_mode(&self) -> u32 {
        self.st_mode
    }
}

/// Encode a [`LinuxStat`] from a file's `size` and `mode`.
///
/// `size` is placed in `st_size` and `mode` (which already carries the file-type
/// bits such as [`S_IFREG`]) in `st_mode`. `st_blksize` is set to a sane value and
/// every other field is zeroed (R2.8).
pub fn encode_stat(size: u64, mode: u32) -> LinuxStat {
    let mut stat = LinuxStat::zeroed();
    stat.st_size = size as i64;
    stat.st_mode = mode;
    stat.st_blksize = DEFAULT_BLKSIZE;
    stat
}
