"""ISO9660 + El Torito writer for a UEFI-only pagh disc (used by tools/mkimage.py).

WHY NOT xorriso. Building a UEFI-bootable ISO normally means ``xorriso -as
mkisofs --efi-boot-part``. That tool is not part of a base system, and the
machine pagh is developed on does not have it, so this module implements exactly
what firmware needs — no more: a Primary Volume Descriptor, one directory tree, a
path table, and an El Torito boot catalog with a single EFI entry.

WHAT THE FIRMWARE ACTUALLY CHECKS (read from EDK2's
``MdeModulePkg/Universal/Disk/PartitionDxe/ElTorito.c``, because guessing here
cost several non-booting images):

* the Boot Record Volume Descriptor must be at sector 17 with SystemId
  ``EL TORITO SPECIFICATION`` and the catalog's 2 KiB LBA at offset 0x47;
* the catalog's first 32 bytes must be a validation entry: indicator 1, the key
  0xAA55 at offset 30, and a checksum over those 32 bytes that sums to zero;
* **every following 32-byte entry with indicator 0x88 and a non-zero LBA becomes
  a bootable partition** — the platform id is not consulted at all, and the FIRST
  such entry is the default boot entry;
* for no-emulation entries the partition size comes from the entry's sector
  count in 512-byte sectors, EXCEPT that a count below 2 means "the rest of the
  disc" — which is how a boot image gets read as a truncated filesystem if the
  count is a placeholder like the commonly seen 4.

So: one bootable entry, no section headers, and a sector count that covers the
whole boot image exactly.
"""
from __future__ import annotations

import pathlib
import struct
import time

ISO_SECTOR = 2048
SECTOR = 512

# ISO9660 field helpers: every multi-byte field is stored twice, little- and
# big-endian. Getting that wrong is the classic "firmware ignores the disc".


def both16(v: int) -> bytes:
    return struct.pack("<H", v) + struct.pack(">H", v)


def both32(v: int) -> bytes:
    return struct.pack("<I", v) + struct.pack(">I", v)


def dir_record(name: bytes, extent: int, size: int, is_dir: bool) -> bytes:
    """One ISO9660 directory record, padded to an even length."""
    now = time.localtime()
    date = bytes([
        max(0, now.tm_year - 1900), now.tm_mon, now.tm_mday,
        now.tm_hour, now.tm_min, now.tm_sec, 0,
    ])
    rec = bytearray()
    rec += b"\x00"                          # extended attribute length
    rec += both32(extent)                   # extent LBA
    rec += both32(size)                     # data length
    rec += date                             # recording date/time
    rec += b"\x02" if is_dir else b"\x00"   # flags
    rec += b"\x00\x00"                      # file unit size, interleave gap
    rec += both16(1)                        # volume sequence number
    rec += bytes([len(name)]) + name
    # ISO9660 stores the length of the WHOLE record — the length byte itself
    # included — in byte 0, and requires that total to be even. So the padding is
    # computed for `1 + len(rec)`, and byte 0 is that same total. Writing
    # `len(rec)` instead (one short) shifts the reader into the middle of every
    # record: the tree then lists a single bogus entry and a bootloader reports
    # "no volume contained a Limine configuration file".
    if (1 + len(rec)) % 2:
        rec += b"\x00"
    total = 1 + len(rec)
    return bytes([total]) + bytes(rec)


def path_table(entries: list[tuple[int, bytes]], big: bool) -> bytes:
    """ISO9660 path table: the L (little-endian) or M (big-endian) variant."""
    out = bytearray()
    for parent, name in entries:
        out += bytes([len(name), 0])
        out += struct.pack(">I", 0) if big else struct.pack("<I", 0)
        out += struct.pack(">H", parent) if big else struct.pack("<H", parent)
        out += name
        if len(name) % 2:
            out += b"\x00"
    return bytes(out)


def build_iso(dest: pathlib.Path, boot_image: pathlib.Path, files: list[tuple[pathlib.Path, str]],
              volume_id: str = "PAGH_OS") -> None:
    """Write an El Torito UEFI ISO.

    `boot_image` is a FAT image (the ESP the firmware mounts and runs
    `\\EFI\\BOOT\\BOOTX64.EFI` from); `files` are placed on the ISO9660 volume as
    `(source, iso_path)` with upper-case 8.3 names, which is where Limine reads
    the kernel and any module from.
    """
    iso_files: list[tuple[bytes, bytes]] = []
    for src, iso_path in files:
        name = pathlib.PurePosixPath(iso_path).name.upper()
        if "." in name:
            stem, ext = name.rsplit(".", 1)
            name = f"{stem[:8]}.{ext[:3]};1"
        else:
            name = f"{name[:8]};1"
        iso_files.append((name.encode(), src.read_bytes()))

    boot_bytes = boot_image.read_bytes()
    # Round the image up to an ISO sector so the partition the firmware derives
    # from the sector count is exactly the image.
    pad = (-len(boot_bytes)) % ISO_SECTOR
    boot_bytes += b"\x00" * pad
    boot_sectors = len(boot_bytes) // SECTOR
    if boot_sectors > 0xFFFF:
        raise SystemExit(
            f"mkimage: boot image is {len(boot_bytes) >> 20} MiB; an El Torito "
            f"sector count can address at most 32 MiB")

    layout = [16, 3]                                  # system area + 3 descriptors
    nsectors = 16 + 3
    l_path = path_table([(1, b"\x00"), (1, b"EFI"), (2, b"BOOT")], big=False)
    m_path = path_table([(1, b"\x00"), (1, b"EFI"), (2, b"BOOT")], big=True)
    l_path_lba = nsectors
    m_path_lba = l_path_lba + (len(l_path) + ISO_SECTOR - 1) // ISO_SECTOR
    nsectors = m_path_lba + (len(m_path) + ISO_SECTOR - 1) // ISO_SECTOR
    del layout

    def alloc(data: bytes) -> int:
        nonlocal nsectors
        lba = nsectors
        nsectors += max(1, (len(data) + ISO_SECTOR - 1) // ISO_SECTOR)
        return lba

    root_lba = alloc(b"")
    efi_lba = alloc(b"")
    boot_dir_lba = alloc(b"")
    loader_off = None
    file_lbas: list[int] = []
    for i, (_, data) in enumerate(iso_files):
        lba = alloc(data)
        file_lbas.append(lba)
        if iso_files[i][0].startswith(b"BOOTX64"):
            loader_off = i
    boot_lba = alloc(boot_bytes)
    catalog_lba = alloc(b"\x00" * ISO_SECTOR)

    # The loader must also live under /EFI/BOOT on the ISO itself (Limine's
    # config search starts next to the binary).
    entries_root = (
        dir_record(b"\x00", root_lba, ISO_SECTOR, True)
        + dir_record(b"\x01", root_lba, ISO_SECTOR, True)
        + dir_record(b"EFI", efi_lba, ISO_SECTOR, True)
    )
    for i, (name, data) in enumerate(iso_files):
        entries_root += dir_record(name, file_lbas[i], len(data), False)
    efi_dir = (
        dir_record(b"\x00", efi_lba, ISO_SECTOR, True)
        + dir_record(b"\x01", root_lba, ISO_SECTOR, True)
        + dir_record(b"BOOT", boot_dir_lba, ISO_SECTOR, True)
    )
    boot_dir = (
        dir_record(b"\x00", boot_dir_lba, ISO_SECTOR, True)
        + dir_record(b"\x01", efi_lba, ISO_SECTOR, True)
    )
    if loader_off is not None:
        boot_dir += dir_record(iso_files[loader_off][0], file_lbas[loader_off],
                               len(iso_files[loader_off][1]), False)

    # El Torito catalog: validation entry + ONE bootable no-emulation entry.
    catalog = bytearray(ISO_SECTOR)
    catalog[0] = 0x01                                  # catalog indicator
    catalog[1] = 0xEF                                  # platform: UEFI
    catalog[4:28] = b"PAGH UEFI BOOT".ljust(24, b"\x00")
    catalog[30:32] = b"\x55\xaa"                       # key, checked by EDK2
    words = struct.unpack("<16H", bytes(catalog[0:32]))
    catalog[28:30] = struct.pack("<H", (-sum(words)) & 0xFFFF)
    catalog[32] = 0x88                                 # bootable
    catalog[33] = 0x00                                 # no emulation
    catalog[34:36] = struct.pack("<H", 0)              # load segment
    catalog[36] = 0x00                                 # system type
    catalog[38:40] = struct.pack("<H", boot_sectors)   # exact size in 512B units
    catalog[40:44] = struct.pack("<I", boot_lba)       # 2 KiB LBA of the image

    pvd = bytearray(ISO_SECTOR)
    pvd[0] = 0x01
    pvd[1:6] = b"CD001"
    pvd[6] = 0x01
    pvd[8:40] = b"PAGH".ljust(32, b" ")
    pvd[40:72] = volume_id.encode().ljust(32, b" ")
    pvd[80:88] = both32(nsectors)
    pvd[120:124] = both16(1)
    pvd[124:128] = both16(1)
    pvd[128:132] = both16(ISO_SECTOR)
    pvd[132:140] = both32(len(l_path))
    pvd[140:144] = struct.pack("<I", l_path_lba)
    pvd[144:148] = struct.pack("<I", 0)
    pvd[148:152] = struct.pack(">I", m_path_lba)
    pvd[152:156] = struct.pack(">I", 0)
    # The PVD's root-directory record is a FIXED 34-byte field. The record built
    # by `dir_record` is exactly that for the root (name = one byte), but it is
    # assigned with an explicit length assertion: a shorter value silently
    # resizes the bytearray and shifts every following PVD field by a byte, which
    # parses as a valid volume descriptor with a zero root extent.
    root_record = dir_record(b"\x00", root_lba, ISO_SECTOR, True)
    assert len(root_record) == 34, f"root directory record is {len(root_record)} bytes, must be 34"
    pvd[156:190] = root_record
    now = time.localtime()
    stamp = (f"{now.tm_year:04d}{now.tm_mon:02d}{now.tm_mday:02d}"
             f"{now.tm_hour:02d}{now.tm_min:02d}{now.tm_sec:02d}00").encode()
    pvd[813:830] = stamp.ljust(17, b"\x00")
    pvd[830:847] = stamp.ljust(17, b"\x00")
    pvd[847:864] = b"0" * 16 + b"\x00"
    pvd[864:881] = stamp.ljust(17, b"\x00")
    pvd[881:898] = stamp.ljust(17, b"\x00")
    pvd[898] = 0x01

    boot_rec = bytearray(ISO_SECTOR)
    boot_rec[0] = 0x00
    boot_rec[1:6] = b"CD001"
    boot_rec[6] = 0x01
    boot_rec[7:39] = b"EL TORITO SPECIFICATION".ljust(32, b"\x00")
    boot_rec[71:75] = struct.pack("<I", catalog_lba)

    term = bytearray(ISO_SECTOR)
    term[0] = 0xFF
    term[1:6] = b"CD001"
    term[6] = 0x01

    image = bytearray(nsectors * ISO_SECTOR)
    for i, blob in enumerate([pvd, boot_rec, term]):
        off = (16 + i) * ISO_SECTOR
        image[off:off + ISO_SECTOR] = bytes(blob)
    placed: list[tuple[int, bytes]] = [
        (l_path_lba, l_path), (m_path_lba, m_path),
        (root_lba, entries_root), (efi_lba, efi_dir), (boot_dir_lba, boot_dir),
        (boot_lba, boot_bytes), (catalog_lba, bytes(catalog)),
    ]
    for i, (_, data) in enumerate(iso_files):
        placed.append((file_lbas[i], data))
    for lba, blob in placed:
        off = lba * ISO_SECTOR
        image[off:off + len(blob)] = blob

    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(bytes(image))
    print(f"[mkimage] ISO image: {dest} ({dest.stat().st_size // (1024 * 1024)} MiB, "
          f"boot image {len(boot_bytes) >> 10} KiB = {boot_sectors} sectors, "
          f"{len(iso_files)} file(s) on the volume)")
