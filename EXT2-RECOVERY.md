# ext2 recovery and host import

This pagh-only patch makes boot formatting non-destructive, reserves bitmap padding correctly, rounds inode geometry for host compatibility and reconciles free-space counters from bitmaps on mount. An existing ext2 with a missing/corrupt pagh WAL is now rejected instead of erased.

## Multi-group layout

The filesystem now spans multiple 32768-block groups sized from the real device capacity
(a 1 GiB disk formats to ≈8 groups), with backup superblock and group-descriptor copies
per group. Free-block/inode counters are reconciled from the bitmaps on every mount.

## Journal capacity vs. large writes

The WAL journal area is fixed at 64 blocks, so a single transaction can carry at most
≈62 dirty blocks. `write_file` therefore commits large files in bounded chunks (32 data
blocks plus metadata per transaction) instead of one whole-file transaction. Whole-file
atomicity is not required by any caller: the package installer removes partial files on
error, and each chunk commit is still crash-atomic.
