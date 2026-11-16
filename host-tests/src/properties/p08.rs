// Feature: linux-binary-compat, Property 8: struct stat encoding preserves size and mode

use crate::stat::{encode_stat, S_IFREG};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any `(size, mode, uid, gid)`, `encode_stat` places `size` in `st_size`,
    /// `mode` in `st_mode`, and the owner ids in `st_uid`/`st_gid`; the accessors
    /// recover the inputs.
    #[test]
    fn stat_round_trips_size_mode_and_owner(
        size in any::<u64>(),
        perms in 0u32..0o7777,
        uid in any::<u32>(),
        gid in any::<u32>(),
    ) {
        // Mode carries the regular-file type bits plus permission bits.
        let mode = S_IFREG | perms;

        let stat = encode_stat(size, mode, uid, gid);

        // st_size holds the (i64-reinterpreted) size.
        prop_assert_eq!(stat.stat_size(), size as i64);
        // st_mode holds the mode verbatim, including S_IFREG.
        prop_assert_eq!(stat.stat_mode(), mode);
        prop_assert_ne!(stat.stat_mode() & S_IFREG, 0);
        // The owner the security model reports is the one that was encoded.
        prop_assert_eq!(stat.st_uid, uid);
        prop_assert_eq!(stat.st_gid, gid);
    }
}

/// The wire layout is an ABI contract, not an implementation detail: every
/// `stat(2)`/`fstat(2)`/`newfstatat(2)` writes the same 144-byte x86_64
/// `struct stat`, and a field reorder/type change here would silently hand a
/// Linux binary a corrupt record. Pin the size, every public field offset and
/// the file-type constants against `asm-generic/stat.h`.
#[test]
fn linux_stat_layout_matches_x86_64_abi() {
    use crate::stat::{LinuxStat, DEFAULT_BLKSIZE, S_IFDIR, S_IFLNK, S_IFREG};
    use core::mem::{offset_of, size_of};

    assert_eq!(size_of::<LinuxStat>(), 144, "x86_64 struct stat is 144 bytes");
    assert_eq!(offset_of!(LinuxStat, st_dev), 0);
    assert_eq!(offset_of!(LinuxStat, st_ino), 8);
    assert_eq!(offset_of!(LinuxStat, st_nlink), 16);
    assert_eq!(offset_of!(LinuxStat, st_mode), 24);
    assert_eq!(offset_of!(LinuxStat, st_uid), 28);
    assert_eq!(offset_of!(LinuxStat, st_gid), 32);
    assert_eq!(offset_of!(LinuxStat, st_rdev), 40);
    assert_eq!(offset_of!(LinuxStat, st_size), 48);
    assert_eq!(offset_of!(LinuxStat, st_blksize), 56);
    assert_eq!(offset_of!(LinuxStat, st_blocks), 64);
    assert_eq!(offset_of!(LinuxStat, st_atime), 72);
    assert_eq!(offset_of!(LinuxStat, st_atime_nsec), 80);
    assert_eq!(offset_of!(LinuxStat, st_mtime), 88);
    assert_eq!(offset_of!(LinuxStat, st_mtime_nsec), 96);
    assert_eq!(offset_of!(LinuxStat, st_ctime), 104);
    assert_eq!(offset_of!(LinuxStat, st_ctime_nsec), 112);
    assert_eq!(offset_of!(LinuxStat, __unused), 120);

    assert_eq!(S_IFREG, 0o100000);
    assert_eq!(S_IFDIR, 0o040000);
    assert_eq!(S_IFLNK, 0o120000);
    assert_eq!(DEFAULT_BLKSIZE, 4096);

    // `encode_stat` fills exactly the identity/size fields; every timestamp,
    // link count and block count starts zeroed for the caller to refine.
    let stat = encode_stat(1234, S_IFREG | 0o644, 1000, 1000);
    assert_eq!(stat.st_size, 1234);
    assert_eq!(stat.st_mode, S_IFREG | 0o644);
    assert_eq!(stat.st_uid, 1000);
    assert_eq!(stat.st_gid, 1000);
    assert_eq!(stat.st_blksize, 4096);
    assert_eq!(stat.st_nlink, 0);
    assert_eq!(stat.st_blocks, 0);
    assert_eq!(stat.st_atime, 0);
    assert_eq!(stat.st_mtime_nsec, 0);
}
