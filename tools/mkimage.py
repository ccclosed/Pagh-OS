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
# ISO menu. The kernel and the disk image are on the ISO9660 volume, not in the
# El Torito boot image (which cannot exceed 32 MiB), so they are addressed by
# volume label: Limine matches `fslabel` against ISO 9660 volume identifiers. With
# `boot():` only the config would resolve and the kernel lookup would fail.
ISO_MENU = """timeout: 10
verbose: yes
serial: yes

/pagh OS (ISO, disk image in RAM)
    protocol: limine
    kernel_path: fslabel(PAGH_OS):/pagh.elf
    module_path: fslabel(PAGH_OS):/disk.img
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


def find_xorriso() -> str | None:
    """Locate `xorriso`; `$XORRISO` overrides.

    WHY xorriso AND NOT A HAND-WRITTEN WRITER. A UEFI-bootable disc needs its El
    Torito boot image to reach the bootloader as a FAT volume the bootloader can
    read. An ISO9660 volume written by hand parses correctly — an independent
    reader lists the tree and extracts files — but the disc does not boot:
    firmware starts the loader and Limine reports `[config file not found]`.
    xorriso's `-eltorito-alt-boot -e <fat.img> -no-emul-boot` produces the layout
    firmware and Limine agree on, so the ISO path uses it and fails loudly when it
    is absent rather than shipping a disc that does not boot.
    """
    for cand in (os.environ.get("XORRISO"), shutil.which("xorriso")):
        if cand and pathlib.Path(cand).is_file():
            return cand
    return None


def build_iso_from(dest: str, loader: pathlib.Path, kernel: pathlib.Path,
                   diskimg: pathlib.Path) -> None:
    """Write a UEFI-bootable ISO: FAT El Torito image + kernel/disk on ISO9660.

    Layout and why each part is where it is:

    * `efi.img` — a small FAT image holding `EFI/BOOT/BOOTX64.EFI` and
      `limine.conf`. This is the El Torito boot image: firmware mounts it and runs
      the loader, and Limine reads the config from it. It lives at the ISO root so
      xorriso can reference it.
    * `pagh.elf` and `disk.img` — on the ISO9660 volume, addressed from the config
      as `fslabel(PAGH_OS):/…`. They cannot go in the boot image: an El Torito
      entry describes at most 32 MiB and the kernel alone is 21 MiB. `fslabel` is
      how Limine reaches the ISO9660 volume (its documentation says the label is
      matched against ISO 9660 volume identifiers too).
    """
    xorriso = find_xorriso()
    if xorriso is None:
        fail("xorriso not found: the ISO needs it (see the note in tools/README.md)")

    env = dict(os.environ)
    # A self-contained xorriso (an unpacked package, not an installed one) needs
    # its own shared libraries on the search path; an installed one does not.
    libdir = os.environ.get("XORRISO_LIBDIR")
    if not libdir:
        for cand in (pathlib.Path(xorriso).resolve().parents[1] / "lib64",
                     pathlib.Path(xorriso).resolve().parents[1] / "lib"):
            if cand.is_dir() and any(cand.glob("libisoburn.so*")):
                libdir = str(cand)
                break
    if libdir:
        prev = env.get("LD_LIBRARY_PATH")
        env["LD_LIBRARY_PATH"] = libdir + (":" + prev if prev else "")

    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        (root / "EFI" / "BOOT").mkdir(parents=True)
        shutil.copy2(loader, root / "EFI" / "BOOT" / "BOOTX64.EFI")
        shutil.copy2(kernel, root / "pagh.elf")
        shutil.copy2(diskimg, root / "disk.img")
        conf = root / "limine.conf"
        conf.write_text(ISO_MENU, encoding="utf-8")
        shutil.copy2(conf, root / "EFI" / "BOOT" / "limine.conf")

        # The FAT boot image: 4 MiB is enough for the loader and two copies of the
        # config, and keeps the El Torito sector count far below its 16-bit limit.
        efi = root / "efi.img"
        with open(efi, "wb") as fh:
            fh.truncate(4 * 1024 * 1024)
        run([shutil.which("mformat") or "mformat", "-i", str(efi), "-v", "EFIBOOT", "::"],
            "mformat efi.img")
        mmd_bin = shutil.which("mmd") or "mmd"
        mcopy_bin = shutil.which("mcopy") or "mcopy"
        run([mmd_bin, "-i", str(efi), "::/EFI"], "mmd ::/EFI")
        run([mmd_bin, "-i", str(efi), "::/EFI/BOOT"], "mmd ::/EFI/BOOT")
        run([mcopy_bin, "-i", str(efi), "-o", str(loader), "::/EFI/BOOT/BOOTX64.EFI"],
            "mcopy BOOTX64.EFI")
        run([mcopy_bin, "-i", str(efi), "-o", str(conf), "::/limine.conf"],
            "mcopy limine.conf")
        run([mcopy_bin, "-i", str(efi), "-o", str(conf), "::/EFI/BOOT/limine.conf"],
            "mcopy EFI/BOOT/limine.conf")

        argv = [
            xorriso, "-as", "mkisofs",
            "-o", str(dest),
            "-V", "PAGH_OS",
            "-R", "-J",
            # UEFI-only: one El Torito alternative entry whose image is the FAT
            # volume above. `-b` (the BIOS entry) is deliberately absent — pointing
            # it at an EFI binary makes firmware find no bootable entry at all.
            "-eltorito-alt-boot",
            "-e", "efi.img", "-no-emul-boot",
            "-isohybrid-gpt-basdat",
            str(root),
        ]
        proc = subprocess.run(argv, capture_output=True, text=True, env=env)
        if proc.returncode != 0:
            fail(f"xorriso failed ({proc.returncode}): {(proc.stderr or proc.stdout).strip()[-400:]}")
    size = pathlib.Path(dest).stat().st_size
    print(f"[mkimage] ISO image: {dest} ({size // (1024 * 1024)} MiB, "
          f"El Torito EFI entry, kernel + disk.img on the ISO9660 volume)")


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
