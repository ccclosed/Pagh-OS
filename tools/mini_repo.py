#!/usr/bin/env python3
"""Build (and optionally serve) a tiny Debian-style binary repository for the
Pagh-OS `apt` end-to-end test.

Layout produced under `tools/mini_repo/`:

    dists/stable/main/binary-amd64/Packages        (uncompressed index)
    dists/stable/main/binary-amd64/Packages.gz     (gzip index, what apt uses)
    pool/main/h/hello-pagh/hello-pagh_1.0_amd64.deb (a real .deb)

The .deb is a real `ar` archive (debian-binary + control.tar.gz + data.tar.gz)
whose `data.tar.gz` installs a single tiny, statically-linked x86_64 Linux ELF at
`usr/bin/hello-pagh`. That ELF does `write(1, "hello from apt\n", 15)` then
`exit_group(0)` via the Linux `int 0x80` ABI — byte-for-byte mirroring the layout
of `src/selftest_lx.rs::build_linux_test_elf`, so the kernel's `run_linux_binary`
loads and runs it.

Usage:
    python tools/mini_repo.py build              # just (re)build the tree
    python tools/mini_repo.py serve [port]       # build, then serve (default 8000)

When serving, the server binds 0.0.0.0 so the QEMU user-net host gateway
(10.0.2.2) reaches it from inside the guest.
"""
import gzip
import hashlib
import io
import os
import struct
import sys
import tarfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.join(HERE, "mini_repo")

HELLO_MSG = b"hello from apt\n"  # 15 bytes


def build_linux_elf(msg: bytes) -> bytes:
    """Hand-assemble a minimal static ET_EXEC x86_64 Linux ELF that writes `msg`
    to stdout and exit_group(0). Mirrors selftest_lx::build_linux_test_elf."""
    VBASE = 0x40_0000
    EHSIZE = 64
    PHSIZE = 56
    code_off = EHSIZE + PHSIZE

    # Fixed-length code (33 bytes), independent of the message contents.
    CODE_LEN = 33
    msg_off = code_off + CODE_LEN
    msg_addr = VBASE + msg_off
    length = len(msg)

    code = bytearray()
    code += bytes([0xB8, 0x01, 0x00, 0x00, 0x00])         # mov eax, 1 (write)
    code += bytes([0xBF, 0x01, 0x00, 0x00, 0x00])         # mov edi, 1 (stdout)
    code += b"\xBE" + struct.pack("<I", msg_addr)          # mov esi, msg_addr
    code += b"\xBA" + struct.pack("<I", length)            # mov edx, len
    code += bytes([0xCD, 0x80])                            # int 0x80
    code += bytes([0xB8, 0xE7, 0x00, 0x00, 0x00])         # mov eax, 231 (exit_group)
    code += bytes([0x31, 0xFF])                            # xor edi, edi
    code += bytes([0xCD, 0x80])                            # int 0x80
    code += bytes([0xEB, 0xFE])                            # jmp $ (fallback)
    assert len(code) == CODE_LEN, len(code)

    entry = VBASE + code_off
    total_len = msg_off + len(msg)

    elf = bytearray()
    # ELF64 header (64 bytes)
    elf += bytes([0x7F, ord('E'), ord('L'), ord('F')])
    elf += bytes([2, 1, 1, 0])           # class64, LSB, version, System V
    elf += bytes(8)                       # ABIVERSION + padding
    elf += struct.pack("<H", 2)           # e_type = ET_EXEC
    elf += struct.pack("<H", 0x3E)        # e_machine = EM_X86_64
    elf += struct.pack("<I", 1)           # e_version
    elf += struct.pack("<Q", entry)       # e_entry
    elf += struct.pack("<Q", EHSIZE)      # e_phoff
    elf += struct.pack("<Q", 0)           # e_shoff
    elf += struct.pack("<I", 0)           # e_flags
    elf += struct.pack("<H", EHSIZE)      # e_ehsize
    elf += struct.pack("<H", PHSIZE)      # e_phentsize
    elf += struct.pack("<H", 1)           # e_phnum
    elf += struct.pack("<H", 0)           # e_shentsize
    elf += struct.pack("<H", 0)           # e_shnum
    elf += struct.pack("<H", 0)           # e_shstrndx
    assert len(elf) == EHSIZE

    # Program header (56 bytes): one PT_LOAD covering the whole image.
    elf += struct.pack("<I", 1)           # PT_LOAD
    elf += struct.pack("<I", 7)           # PF_R|PF_W|PF_X
    elf += struct.pack("<Q", 0)           # p_offset
    elf += struct.pack("<Q", VBASE)       # p_vaddr
    elf += struct.pack("<Q", VBASE)       # p_paddr
    elf += struct.pack("<Q", total_len)   # p_filesz
    elf += struct.pack("<Q", total_len)   # p_memsz
    elf += struct.pack("<Q", 0x1000)      # p_align
    assert len(elf) == EHSIZE + PHSIZE

    elf += bytes(code)
    elf += msg
    assert len(elf) == total_len
    return bytes(elf)


def make_tar_gz(members: list) -> bytes:
    """Build a ustar tar of (name, mode, bytes) members, gzip it, return bytes."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for name, mode, data in members:
            ti = tarfile.TarInfo(name=name)
            ti.size = len(data)
            ti.mode = mode
            ti.mtime = 0
            ti.uid = 0
            ti.gid = 0
            ti.type = tarfile.REGTYPE
            tf.addfile(ti, io.BytesIO(data))
    # Deterministic gzip (mtime=0).
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:
        gz.write(raw.getvalue())
    return out.getvalue()


def ar_member(name: str, data: bytes) -> bytes:
    """Encode one `ar` archive member (60-byte header + content + even pad)."""
    header = b""
    header += name.encode().ljust(16, b" ")     # name (16)
    header += b"0".ljust(12, b" ")               # mtime (12)
    header += b"0".ljust(6, b" ")                # uid (6)
    header += b"0".ljust(6, b" ")                # gid (6)
    header += b"100644".ljust(8, b" ")           # mode (8)
    header += str(len(data)).encode().ljust(10, b" ")  # size (10)
    header += b"\x60\x0a"                         # magic `\n
    assert len(header) == 60, len(header)
    out = header + data
    if len(data) % 2 == 1:
        out += b"\n"
    return out


def build_deb(control_tar_gz: bytes, data_tar_gz: bytes) -> bytes:
    """Assemble a .deb: !<arch> + debian-binary + control.tar.gz + data.tar.gz."""
    out = b"!<arch>\n"
    out += ar_member("debian-binary", b"2.0\n")
    out += ar_member("control.tar.gz", control_tar_gz)
    out += ar_member("data.tar.gz", data_tar_gz)
    return out


def build_repo() -> None:
    elf = build_linux_elf(HELLO_MSG)

    # data.tar.gz: the installed file tree.
    data_tar_gz = make_tar_gz([("usr/bin/hello-pagh", 0o755, elf)])

    # control.tar.gz: a minimal control file (not parsed on install, included for
    # a well-formed .deb so locate_members finds all three members).
    control_text = (
        "Package: hello-pagh\n"
        "Version: 1.0\n"
        "Architecture: amd64\n"
        "Maintainer: Pagh-OS <root@pagh>\n"
        "Description: tiny hello binary for the apt end-to-end test\n"
    ).encode()
    control_tar_gz = make_tar_gz([("control", 0o644, control_text)])

    deb = build_deb(control_tar_gz, data_tar_gz)

    # Write the pool .deb.
    pool_dir = os.path.join(REPO, "pool", "main", "h", "hello-pagh")
    os.makedirs(pool_dir, exist_ok=True)
    deb_name = "hello-pagh_1.0_amd64.deb"
    deb_path = os.path.join(pool_dir, deb_name)
    with open(deb_path, "wb") as f:
        f.write(deb)

    pool_rel = "pool/main/h/hello-pagh/" + deb_name
    size = len(deb)
    md5 = hashlib.md5(deb).hexdigest()
    sha256 = hashlib.sha256(deb).hexdigest()

    # The Packages index. Fields the kernel parser keeps: Package, Version,
    # Architecture, Filename, Depends (none -> trivial install), Size.
    packages = (
        f"Package: hello-pagh\n"
        f"Version: 1.0\n"
        f"Architecture: amd64\n"
        f"Maintainer: Pagh-OS <root@pagh>\n"
        f"Filename: {pool_rel}\n"
        f"Size: {size}\n"
        f"MD5sum: {md5}\n"
        f"SHA256: {sha256}\n"
        f"Description: tiny hello binary for the apt end-to-end test\n"
        f"\n"
    ).encode()

    idx_dir = os.path.join(REPO, "dists", "stable", "main", "binary-amd64")
    os.makedirs(idx_dir, exist_ok=True)
    with open(os.path.join(idx_dir, "Packages"), "wb") as f:
        f.write(packages)
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:
        gz.write(packages)
    with open(os.path.join(idx_dir, "Packages.gz"), "wb") as f:
        f.write(out.getvalue())

    # A minimal Release file (not required by the kernel, included for realism).
    rel_dir = os.path.join(REPO, "dists", "stable")
    with open(os.path.join(rel_dir, "Release"), "wb") as f:
        f.write(
            b"Suite: stable\nComponent: main\nArchitectures: amd64\n"
        )

    print(f"built repo at {REPO}")
    print(f"  .deb        : {pool_rel} ({size} bytes, md5 {md5[:12]}...)")
    print(f"  Packages.gz : dists/stable/main/binary-amd64/Packages.gz")
    print(f"  ELF         : usr/bin/hello-pagh ({len(elf)} bytes), prints {HELLO_MSG!r}")


def serve(port: int) -> None:
    import http.server
    import socketserver

    os.chdir(REPO)
    handler = http.server.SimpleHTTPRequestHandler

    class Server(socketserver.TCPServer):
        allow_reuse_address = True

    with Server(("0.0.0.0", port), handler) as httpd:
        print(f"serving {REPO} at http://0.0.0.0:{port} (guest reaches host at 10.0.2.2)")
        httpd.serve_forever()


def main() -> None:
    mode = sys.argv[1] if len(sys.argv) > 1 else "build"
    build_repo()
    if mode == "serve":
        port = int(sys.argv[2]) if len(sys.argv) > 2 else 8000
        serve(port)


if __name__ == "__main__":
    main()
