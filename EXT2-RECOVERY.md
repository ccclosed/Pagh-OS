# ext2 recovery and host import

This pagh-only patch reserves bitmap padding correctly, rounds inode geometry for host
compatibility and reconciles free-space counters from bitmaps on mount.

## Boot formatting is allowed only on a genuinely blank device

`boot.rs::init_fs` formats a device **only when it is blank**, and the decision is
`fs::format_policy` (issue #33). It used to format whenever `Ext2Fs::mount` failed and the
ext2 magic was absent at byte 1024 — which on a real machine is the machine's *own*
disk: the NVMe path takes the first active namespace (the whole disk, not a partition),
and byte 1024 of a GPT disk holds the partition-entry array, so a partitioned disk was
classified "blank" and erased.

The rules, in order:

1. a **mountable ext2 superblock** (a parsable `read_sb_gds`, not merely the magic) means
   "this is a filesystem" — never format, and an ext2/ext4 volume with a missing or
   corrupt pagh WAL is rejected instead of erased; ext4 shares the `0xEF53` magic and is
   covered by this same rule;
2. an **MBR/GPT partition table** (boot signature at byte 510, or `EFI PART` at LBA 1) is
   refused by name — this is the real-hardware case;
3. any other **non-zero data** in the probed 1 MiB window is refused by name;
4. an **all-zero** device is formatted, and so is any device carrying the explicit
   opt-in marker below.

### Taking over a disk that already has data

Formatting a device that is not blank requires a deliberate, unmistakable act by the
operator — writing the marker into sector 0 of that device:

```sh
printf 'PAGH-FORMAT' | sudo dd of=/dev/nvme0n1 bs=1 conv=notrunc
```

Nothing else enables it: there is no bootloader flag, no config file, and the kernel
never writes those bytes itself. Omit the marker and the device is refused with a line
naming what was found on it. There is deliberately no "format anyway" prompt, because
this kernel boots with no console input available at that point.

Boot probes at most 1 MiB, and only after a mount failure, so the happy path pays
nothing for the check.


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
