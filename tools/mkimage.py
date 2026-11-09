#!/usr/bin/env python3
"""Build the bootable media pagh runs from on real hardware.

The kernel is a Limine UEFI payload, so the image is UEFI-only:

* ``--usb dist/pagh-usb.img`` — a GPT image with one FAT32 EFI System Partition
  holding the loader, the kernel and the disk image. ``dd`` it onto a USB stick
  and any UEFI machine boots it. This is the artifact for a real PC: an optical
  drive is the exception now, a USB port is not.
* ``--iso dist/pagh.iso`` — an ISO9660 disc with an El Torito EFI entry, which
  is what a Ventoy stick (or a virtual machine, or an optical drive) wants. The
  writer is `tools/iso9660.py`; its docstring records the exact conditions
  firmware checks, taken from EDK2's El Torito source, because guessing them cost
  several non-booting images.

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

# ISO menu. The paths use `fslabel` DELIBERATELY: on an El Torito disc `boot()`
# is "the partition holding this config file", which is the small FAT boot image,
# not the ISO9660 volume where the kernel actually lives. Limine documents
# `fslabel` as matching FAT volume labels *and ISO 9660 volume identifiers*, so
# the disc is addressed by the volume id the writer put in the PVD. The second
# entry is the USB-image layout (`boot():`), kept as a fallback.
ISO_MENU = """timeout: 10
verbose: yes
serial: yes

/pagh OS (ISO)
    protocol: limine
    kernel_path: boot():/pagh.elf
    module_path: boot():/disk.img
"""

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


def build_iso_from(dest: str, loader: pathlib.Path, kernel: pathlib.Path,
                   diskimg: pathlib.Path) -> None:
    """Assemble the EFI boot image and hand it to the ISO9660 writer.

    The split matters: the boot image is what firmware mounts and runs
    `EFI/BOOT/BOOTX64.EFI` from, and it must stay under 32 MiB because the El
    Torito sector count is 16 bits. The kernel (21 MiB) and the disk image
    (64 MiB) therefore live on the ISO9660 volume, where Limine reads them.
    """
    import tempfile

    import iso9660

    with tempfile.TemporaryDirectory() as tmp:
        tmpdir = pathlib.Path(tmp)
        conf = tmpdir / "limine.conf"
        conf.write_text(ISO_MENU, encoding="utf-8")
        boot_img = tmpdir / "efiboot.img"
        # THE KERNEL MUST BE IN HERE. Firmware hands the bootloader only the El
        # Torito *boot image* as a partition (an "HD(1,...)" child of the CD
        # device); the ISO9660 volume is a separate device Limine does not
        # necessarily see, so `boot():/pagh.elf` resolves on the FAT image and
        # nowhere else. A 1.44 MB floppy could not hold a 21 MiB kernel, hence
        # FAT16 in a 30 MiB image: still inside the 0xFFFF-sector (32 MiB) limit
        # the El Torito entry can describe, and big enough for loader + kernel.
        payload = kernel.stat().st_size + diskimg.stat().st_size
        boot_mb = max(2, (payload + (1 << 20) - 1) >> 20)
        if boot_mb * 1024 * 1024 // 512 > 0xFFFF:
            fail(f"the ISO boot image would be {boot_mb} MiB, but an El Torito "
                 f"entry can describe at most 32 MiB: pass a smaller --disk image "
                 f"(the kernel alone is {kernel.stat().st_size >> 20} MiB)")
        with open(boot_img, "wb") as fh:
            fh.truncate(boot_mb * 1024 * 1024)
        run([shutil.which("mformat") or "mformat", "-i", str(boot_img), "-v",
             "EFIBOOT", "::"], "mformat efiboot")
        mmd_bin = shutil.which("mmd") or "mmd"
        mcopy_bin = shutil.which("mcopy") or "mcopy"
        run([mmd_bin, "-i", str(boot_img), "::/EFI"], "mmd ::/EFI")
        run([mmd_bin, "-i", str(boot_img), "::/EFI/BOOT"], "mmd ::/EFI/BOOT")
        run([mcopy_bin, "-i", str(boot_img), "-o", str(loader), "::/EFI/BOOT/BOOTX64.EFI"],
            "mcopy BOOTX64.EFI")
        # The config goes next to the EFI binary as well as at the volume root:
        # a UEFI Limine looks for `limine.conf` beside BOOTX64.EFI first, and a
        # disc whose only config is at /limine.conf boots into an empty menu.
        run([mcopy_bin, "-i", str(boot_img), "-o", str(conf), "::/limine.conf"],
            "mcopy limine.conf")
        run([mcopy_bin, "-i", str(boot_img), "-o", str(conf), "::/EFI/BOOT/limine.conf"],
            "mcopy EFI/BOOT/limine.conf")
        run([mcopy_bin, "-i", str(boot_img), "-o", str(kernel), "::/pagh.elf"],
            "mcopy pagh.elf")
        run([mcopy_bin, "-i", str(boot_img), "-o", str(diskimg), "::/disk.img"],
            "mcopy disk.img")
        iso9660.build_iso(
            pathlib.Path(dest),
            boot_img,
            [
                (loader, "EFI/BOOT/BOOTX64.EFI"),
                (conf, "limine.conf"),
                (kernel, "pagh.elf"),
                (diskimg, "disk.img"),
            ],
            volume_id="PAGH_OS",
        )


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
    ap.add_argument("--iso", metavar="PATH", help="write an El Torito UEFI ISO here")
    ap.add_argument("--disk", metavar="PATH", help="disk image to embed (default disk.img)")
    ap.add_argument("--elf", metavar="PATH", help="kernel ELF (default iso_root/pagh.elf)")
    ap.add_argument("--size-mb", type=int, default=0,
                    help="minimum USB image size (default: sized to the payload)")
    args = ap.parse_args()
    if not args.usb and not args.iso:
        ap.error("nothing to do: pass --usb PATH and/or --iso PATH")

    kernel, diskimg, loader = resolve_inputs(args.elf, args.disk)
    log(f"kernel {kernel} ({kernel.stat().st_size // 1024} KiB)")
    log(f"disk   {diskimg} ({diskimg.stat().st_size // (1024 * 1024)} MiB)")
    log(f"loader {loader}")
    if args.usb:
        build_usb(pathlib.Path(args.usb), kernel, diskimg, loader, args.size_mb or 0)
    if args.iso:
        build_iso_from(args.iso, loader, kernel, diskimg)
    return 0


if __name__ == "__main__":
    sys.exit(main())
