//! Storage filesystem subsystem.
//!
//! Hosts the ext2-compatible filesystem (`fs::ext2`) and the write-ahead-log
//! journal (`fs::journal`) described in the `networking-and-storage` spec.
//!
//! Task 4 implements the entire ext2 + WAL journal layer as **pure logic** that
//! is exercised over a RAM-mock `BlockDevice` (see `crate::test`); it carries no
//! real-disk dependency and is not wired into boot or the VFS mount table here
//! (that is Task 5).

pub mod ext2;
pub mod journal;

/// Errors produced by the filesystem subsystem.
///
/// Mirrors the error model in the design document (`FsError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// No virtio-blk device was discovered, so storage is unavailable.
    NoDevice,
    /// The on-disk ext2 superblock is missing/invalid (`s_magic != 0xEF53`) or
    /// declares a block size that does not match the compiled `BS`.
    BadSuperBlock,
    /// The WAL journal superblock magic did not validate.
    BadJournal,
    /// A bitmap (block or inode) had no free bit left.
    OutOfSpace,
    /// A path component / inode / directory entry was not found.
    NotFound,
    /// An entry with the same name already exists in the directory.
    AlreadyExists,
    /// The underlying block device returned an error.
    IoError,
    /// A directory entry name exceeded 255 bytes.
    NameTooLong,
    /// A structural invariant was violated (corrupt/out-of-range field).
    Corrupt,
}
