#!/usr/bin/env python3
"""Build the bootable media pagh runs from on real hardware.

The kernel is a Limine UEFI payload, so the image is UEFI-only:

* ``--usb dist/pagh-usb.img`` — a GPT image with one FAT32 EFI System Partition
  holding the loader, the kernel and the disk image. ``dd`` it onto a USB stick
  and any UEFI machine boots it. This is the artifact for a real PC: an optical
  drive is the exception now, a USB port is not.
There is deliberately NO ISO builder here. A UEFI-bootable ISO needs El Torito,
and the authoring tool for that is xorriso, which is not part of a base system
(and not present on the machine this was written on). A hand-built ISO9660 +
El Torito catalog was tried and OVMF rejects it ("Default entry is not valid,
booting to menu"), so instead of shipping a builder that emits a disc nothing
boots, the recipe with the real tool is documented in ``tools/README.md``. The
USB image below is the artifact that actually works on hardware.

WHY THE DISK IMAGE IS ON THE MEDIA. The kernel mounts virtio-blk or NVMe and
nothing else, so a PC with only SATA comes up with no ``/mnt`` — a shell and no
programs. Limine loads ``disk.img`` next to the kernel and ``drivers::memdisk``
presents it as a block device, so the medium carries the system. Writes go to
RAM and are lost on reboot; the boot menu says so.

WHY NOT xorriso. Building a UEFI-bootable ISO normally means ``xorriso -as
mkisofs --efi-boot-part``. That tool is not part of a base system, so the ISO
writer here is a small, self-contained implementation of exactly what the
firmware needs (ISO9660 primary volume descriptor + El Torito EFI entry). It has
no dependency beyond the Python standard library and the repo's own Limine tree.

The FAT partition is written with ``mtools`` (``mformat``/``mcopy``), which is
the one external tool required — and it is only needed for the partition
formatting, not for the ISO layout.

Usage:
    python3 tools/mkimage.py --usb dist/pagh-usb.img
    python3 tools/mkimage.py --iso dist/pagh.iso
    python3 tools/mkimage.py --usb dist/pagh-usb.img --disk disk.img --elf iso_root/pagh.elf
"""
from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

SECTOR = 512
ISO_SECTOR = 2048
# The partition starts at 1 MiB: the conventional, firmware-friendly alignment.
ESP_OFFSET = 1024 * 1024

BOOT_MENU = """timeout: 5
verbose: yes
serial: yes

/pagh OS (RAM disk)
    protocol: limine
    kernel_path: boot():/pagh.elf
    module_path: boot():/disk.img
"""


def log(msg: str) -> None:
    print(f"[mkimage] {msg}", flush=True)


def fail(msg: str) -> "NoReturn":  # type: ignore[name-defined]
    print(f"[mkimage] error: {msg}", file=sys.stderr, flush=True)
    raise SystemExit(1)


def resolve_inputs(elf: str | None, disk: str | None) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
    """Locate the kernel, the disk image and the Limine UEFI loader."""
    import limine  # tools/limine.py — the repo's version-agnostic locator

    kernel = pathlib.Path(elf) if elf else ROOT / "iso_root" / "pagh.elf"
    if not kernel.is_file():
        fail(f"kernel not found: {kernel}\n  run `python3 tools/build.py stage --release` first")
    diskimg = pathlib.Path(disk) if disk else ROOT / "disk.img"
    if not diskimg.is_file():
        fail(f"disk image not found: {diskimg}\n"
             f"  create one (64 MiB ext2 with the userland) or pass --disk")
    loader = limine.ensure(ROOT, None)
    return kernel, diskimg, pathlib.Path(loader)


def fat_partition(dest: pathlib.Path, payload: list[tuple[pathlib.Path, str]],
                  size_mb: int) -> None:
    """Write a FAT32 filesystem at ``ESP_OFFSET`` inside ``dest``.

    ``mtools`` addresses the filesystem through ``image@@offset``, which is what
    lets a partitioned image be filled without loop devices or root: the file is
    a plain file, and the offset is part of the drive spec.
    """
    mformat = shutil.which("mformat")
    mcopy = shutil.which("mcopy")
    mmd = shutil.which("mmd")
    if not (mformat and mcopy and mmd):
        fail("mtools is required (mformat/mcopy/mmd); install `mtools`")
    spec = f"{dest}@@{ESP_OFFSET}"
    run([mformat, "-i", spec, "-F", "-v", "PAGHESP", "::"], "mformat")
    made: set[str] = set()
    for src, guest in payload:
        parent = str(pathlib.PurePosixPath(guest).parent)
        if parent not in (".", "/") and parent not in made:
            # `mmd` has no -p: create each level, and never twice (a second mmd
            # on an existing directory is a hard error, not a no-op).
            parts = pathlib.PurePosixPath(parent).parts
            for i in range(1, len(parts) + 1):
                sub = "/".join(parts[:i])
                if sub in made:
                    continue
                run([mmd, "-i", spec, f"::{sub}"], f"mmd {sub}")
                made.add(sub)
        run([mcopy, "-i", spec, "-o", str(src), f"::{guest}"], f"mcopy {guest}")


def run(argv: list[str], what: str) -> None:
    proc = subprocess.run(argv, capture_output=True, text=True)
    if proc.returncode != 0:
        fail(f"{what} failed ({proc.returncode}): {(proc.stderr or proc.stdout).strip()}")


def build_usb(dest: pathlib.Path, kernel: pathlib.Path, diskimg: pathlib.Path,
              loader: pathlib.Path, size_mb: int) -> None:
    """GPT + one FAT32 ESP, sized to hold the loader, the kernel and the disk."""
    needed = (ESP_OFFSET + kernel.stat().st_size + diskimg.stat().st_size) // (1024 * 1024)
    total_mb = max(size_mb, needed + 16)
    dest.parent.mkdir(parents=True, exist_ok=True)
    with open(dest, "wb") as fh:
        fh.truncate(total_mb * 1024 * 1024)
    # GPT with a protective MBR; parted writes both.
    run(["parted", "-s", str(dest), "mklabel", "gpt"], "parted mklabel")
    run(["parted", "-s", str(dest), "mkpart", "ESP", "fat32",
         f"{ESP_OFFSET // 512}s", "100%"], "parted mkpart")
    run(["parted", "-s", str(dest), "set", "1", "esp", "on"], "parted set esp")

    with tempfile.TemporaryDirectory() as tmp:
        tmpdir = pathlib.Path(tmp)
        conf = tmpdir / "limine.conf"
        conf.write_text(BOOT_MENU, encoding="utf-8")
        fat_partition(dest, [
            (loader, "EFI/BOOT/BOOTX64.EFI"),
            (conf, "limine.conf"),
            (conf, "EFI/BOOT/limine.conf"),
            (kernel, "pagh.elf"),
            (diskimg, "disk.img"),
        ], total_mb)
    log(f"USB image: {dest} ({dest.stat().st_size // (1024 * 1024)} MiB, "
        f"ESP at {ESP_OFFSET // 1024} KiB, GPT)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--usb", metavar="PATH", help="write a GPT+ESP USB image here")
    ap.add_argument("--disk", metavar="PATH", help="disk image to embed (default disk.img)")
    ap.add_argument("--elf", metavar="PATH", help="kernel ELF (default iso_root/pagh.elf)")
    ap.add_argument("--size-mb", type=int, default=0,
                    help="minimum USB image size (default: sized to the payload)")
    args = ap.parse_args()
    if not args.usb:
        ap.error("nothing to do: pass --usb PATH")

    kernel, diskimg, loader = resolve_inputs(args.elf, args.disk)
    log(f"kernel {kernel} ({kernel.stat().st_size // 1024} KiB)")
    log(f"disk   {diskimg} ({diskimg.stat().st_size // (1024 * 1024)} MiB)")
    log(f"loader {loader}")
    if args.usb:
        build_usb(pathlib.Path(args.usb), kernel, diskimg, loader, args.size_mb or 0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
