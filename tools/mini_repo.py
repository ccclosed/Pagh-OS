#!/usr/bin/env python3
"""Build (and optionally serve) the tiny Debian-style repository used by the apt
end-to-end tests, including the SIGNED and deliberately-broken suites that prove
issue #32's trust chain.

Suites under `tools/mini_repo/dists/` (one served root, switched with `apt setsuite`):

    stable/          correct AND signed          -> `apt update` + `install` succeed
    tampered-index/  signature VALID, but the served `Packages` is a same-length
                     version-swapped copy of the one the signed `Release` describes
                                                  -> update refused, no index published
    tampered-deb/    metadata correct and signed, but the served `.deb` is not the
                     one the signed `Packages` describes
                                                  -> install refused BEFORE unpacking
    unsigned/        no `InRelease`, no `Release.gpg`
                                                  -> update refused (visible)
    untrusted/       signed by a key that is NOT in the trust anchor
                                                  -> update refused (no trusted signature)

The verification suites each serve their own `pool/<suite>/...` `.deb`, so the
cases cannot interfere; `stable` keeps the Debian-style `pool/main/...` layout it
had before the mirror became multi-suite, so its committed artifacts stay
byte-for-byte unchanged. The signatures come from `tools/openpgp_sign.py` using the
deterministic TEST-ONLY seeds in `tools/gen_openpgp_testkey.py`: no committed
secret, no `gpg` on the host, byte-identical output on every run. The kernel
verifies them against `src/pkg/openpgp_test_keys.rs`, compiled ONLY into the
`lx_selftest` / `lx_bigindex` harness builds.

The `.deb` payload is a real `ar` archive (debian-binary + control.tar.gz +
data.tar.gz) whose `data.tar.gz` installs a tiny statically-linked x86_64 Linux
ELF at `usr/bin/hello-pagh` (it writes `hello from apt` and exits) — the same
layout as `src/selftest_lx.rs::build_linux_test_elf`.

Usage:
    python3 tools/mini_repo.py build              # just (re)build the tree
    python3 tools/mini_repo.py serve [port]       # build, then serve (default 8000)

When serving, the server binds 0.0.0.0 so the QEMU user-net host gateway
(10.0.2.2) reaches it from inside the guest.
"""
import gzip
import hashlib
import io
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import openpgp_sign as pgp  # noqa: E402  (deterministic test signing)
import gen_openpgp_testkey as testkey  # noqa: E402  (the TEST-ONLY seeds/UIDs)
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


def make_tar_gz(members: list, fmt: int = tarfile.USTAR_FORMAT) -> bytes:
    """Build a tar from explicit members, gzip it, return bytes.

    `members` are dicts: `name`, `mode`, and either `data` (a regular file) or
    `linkname` plus `type` (`tarfile.SYMTYPE`/`tarfile.LNKTYPE`). The default is
    ustar, which is what the ordinary payloads use; the caller passes
    `tarfile.GNU_FORMAT` when a member needs GNU extensions — it is what dpkg's
    `tar` writes, so a member path longer than the 100-byte `name` field comes
    out as a `'L'` header, exactly the encoding the kernel parser has to
    understand (issue #18).
    """
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=fmt) as tf:
        for m in members:
            ti = tarfile.TarInfo(name=m["name"])
            ti.mode = m.get("mode", 0o644)
            ti.mtime = 0
            ti.uid = 0
            ti.gid = 0
            if "linkname" in m:
                ti.type = m.get("type", tarfile.SYMTYPE)
                ti.linkname = m["linkname"]
                ti.size = 0
                tf.addfile(ti)
            else:
                data = m.get("data", b"")
                ti.type = tarfile.REGTYPE
                ti.size = len(data)
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


# The suites served from one mirror root. `signer` is None for the deliberately
# unsigned suite, "untrusted" for the second (never-anchored) key, "test" for the
# key the `lx_selftest` build anchors.
SUITES = [
    ("stable", "test"),
    ("tampered-index", "test"),
    ("tampered-deb", "test"),
    ("unsigned", None),
    ("untrusted", "untrusted"),
    # A complete, correctly signed OLDER triplet. Nothing in the chain can tell it
    # from the current one (the `stable` suite has no `Valid-Until`), so `apt`
    # ACCEPTS it — the harness prints a NOTE for it instead of pretending the
    # rollback is caught. See SECURITY.md, "What is still not verified".
    ("stale", "test"),
]

RELEASE_DATE = "Thu, 01 Jan 2026 00:00:00 UTC"


def gzip_bytes(data: bytes) -> bytes:
    """Deterministic gzip (mtime=0): the committed tree must be reproducible."""
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:
        gz.write(data)
    return out.getvalue()


def pool_dir(suite: str, letter: str, pkg: str) -> str:
    """The pool directory for one package of one suite, relative to `REPO`.

    `stable` keeps the Debian-style `pool/main/...` layout that predates the
    multi-suite mirror (issue #18's `links-pagh` fixture lives there); the
    verification suites added for issue #32 use `pool/<suite>/...` so one
    generator can serve several contradicting release triplets at once without
    their payloads colliding.
    """
    top = "main" if suite == "stable" else suite
    return os.path.join(REPO, "pool", top, letter, pkg)


def stanza(
    pkg: str, suite: str, deb: bytes, version: str, description: str
) -> bytes:
    """One `Packages` stanza for `pkg`, pointing at its pool path.

    `MD5sum` is carried for Debian-index realism only: the kernel verifies the
    `SHA256`/`Size` pair from the signed index and never reads MD5 (see
    `src/pkg/apt_index.rs`), but a real mirror's stanza has it.
    """
    letter = pkg[0]
    top = "main" if suite == "stable" else suite
    rel = f"pool/{top}/{letter}/{pkg}/{pkg}_{version}_amd64.deb"
    return (
        f"Package: {pkg}\n"
        f"Version: {version}\n"
        f"Architecture: amd64\n"
        f"Maintainer: Pagh-OS <root@pagh>\n"
        f"Filename: {rel}\n"
        f"Size: {len(deb)}\n"
        f"MD5sum: {hashlib.md5(deb).hexdigest()}\n"
        f"SHA256: {hashlib.sha256(deb).hexdigest()}\n"
        f"Description: {description}\n"
        f"\n"
    ).encode()


def packages_stanza(
    suite: str, deb: bytes, version: str = "1.0", links: bytes | None = None
) -> bytes:
    """The `Packages` index for one suite: `hello-pagh`, plus the `links-pagh`
    symlink/hardlink fixture (issue #18) on the suites that serve it.

    `links` is the raw `.deb`; `None` means this suite has no second package
    (the stale triplet is deliberately older-only, and the tampered suites stay
    minimal so a failure has exactly one candidate cause).
    """
    out = stanza(
        "hello-pagh",
        suite,
        deb,
        version,
        "tiny hello binary for the apt end-to-end test",
    )
    if links is not None:
        out += stanza(
            "links-pagh",
            suite,
            links,
            "1.0",
            "symlink and hardlink fixture for issue #18",
        )
    return out


def attacker_index(honest: bytes) -> bytes:
    """A length-preserving tamper of an index: the attacker swaps the package
    version, without changing the file's size (so the signed `Size:` still matches
    and the *hash* is the only thing that can catch it).

    Both versions this fixture can carry must be swapped, because the index must
    come out byte-different for the suite to demonstrate anything: `1.0` for the
    current suites, `0.9` for `stale`. A version the honest index does not contain
    is not an error — there is simply nothing to replace — so no assertion is made
    about *which* substitution fired, only that the result is the same length and
    (checked by the caller's expectations) a different digest.
    """
    out = honest
    for was in (b"Version: 1.0\n", b"Version: 0.9\n"):
        out = out.replace(was, b"Version: 6.6\n")
    assert len(out) == len(honest), "version swap must be length-preserving"
    assert out != honest, "the tamper must change the index"
    return out


def suite_release(
    suite: str, packages: bytes, packages_gz: bytes | None, date: str = RELEASE_DATE
) -> bytes:
    entries = [f" {hashlib.sha256(packages).hexdigest()} {len(packages)} main/binary-amd64/Packages\n"]
    if packages_gz is not None:
        entries.append(
            f" {hashlib.sha256(packages_gz).hexdigest()} {len(packages_gz)} main/binary-amd64/Packages.gz\n"
        )
    return (
        f"Suite: {suite}\n"
        f"Codename: pagh-test-{suite}\n"
        f"Date: {date}\n"
        f"Architectures: all amd64\n"
        f"Components: main\n"
        f"Acquire-By-Hash: no\n"
        f"\n"
        f"SHA256:\n" + "".join(entries)
    ).encode()


def sign_release(rel_dir: str, release: bytes, signer) -> None:
    """Write `Release.gpg` + `InRelease` for a suite, or nothing if unsigned."""
    if signer is None:
        return
    seed = testkey.SEED_SIGNING if signer == "test" else testkey.SEED_UNTRUSTED
    key_body = pgp.public_key_packet(seed, testkey.CREATED)
    detached = pgp.signature_packet(seed, key_body, release, 0x00, testkey.CREATED)
    with open(os.path.join(rel_dir, "Release.gpg"), "wb") as f:
        f.write(pgp.armor(detached))
    # The clear-signed signature covers the CANONICAL text (CRLF joins, no final
    # line ending), never the raw document — see `openpgp_sign.canonicalize`.
    cleartext = pgp.signature_packet(
        seed, key_body, pgp.canonicalize(release), 0x01, testkey.CREATED
    )
    with open(os.path.join(rel_dir, "InRelease"), "wb") as f:
        f.write(pgp.cleartext_signed(release, cleartext))


def stale_deb() -> bytes:
    """An older, differently-versioned payload for the rollback demonstration."""
    elf = build_linux_elf(b"hello from apt\n")
    data_tar_gz = make_tar_gz([{"name": "usr/bin/hello-pagh", "mode": 0o755, "data": elf}])
    control_text = (
        "Package: hello-pagh\n"
        "Version: 0.9\n"
        "Architecture: amd64\n"
        "Maintainer: Pagh-OS <root@pagh>\n"
        "Description: older hello binary (rollback demonstration)\n"
    ).encode()
    control_tar_gz = make_tar_gz([{"name": "control", "mode": 0o644, "data": control_text}])
    return build_deb(control_tar_gz, data_tar_gz)


# 2025-06-01: an older (and still correct) repository revision.
STALE_DATE = "Sun, 01 Jun 2025 00:00:00 UTC"


def write_suite(suite: str, signer, deb: bytes, tamper_deb: bool, links: bytes = None) -> None:
    version = "0.9" if suite == "stale" else "1.0"
    date = STALE_DATE if suite == "stale" else RELEASE_DATE
    hello_dir = pool_dir(suite, "h", "hello-pagh")
    os.makedirs(hello_dir, exist_ok=True)
    served_deb = bytearray(deb)
    if tamper_deb:
        # Flip one bit inside the (compressed) data member: the file keeps its
        # exact length, so the signed `Size:` matches and the SHA-256 is the only
        # check that can refuse it. `apt install` verifies the digest BEFORE it
        # parses the archive, which is the behaviour under test.
        served_deb[-1] ^= 0x01
    served_deb = bytes(served_deb)
    with open(os.path.join(hello_dir, f"hello-pagh_{version}_amd64.deb"), "wb") as f:
        f.write(served_deb)

    if links is not None:
        links_dir = pool_dir(suite, "l", "links-pagh")
        os.makedirs(links_dir, exist_ok=True)
        with open(os.path.join(links_dir, "links-pagh_1.0_amd64.deb"), "wb") as f:
            f.write(links)

    # The index always describes the HONEST payload; only the tampered-deb suite
    # then serves a different file, which is exactly the case the digest check
    # must catch. (For every other suite the served file is the honest one.)
    honest = packages_stanza(suite, deb, version, links)
    honest_gz = gzip_bytes(honest)

    idx_dir = os.path.join(REPO, "dists", suite, "main", "binary-amd64")
    os.makedirs(idx_dir, exist_ok=True)
    with open(os.path.join(idx_dir, "Packages"), "wb") as f:
        f.write(honest)
    served_gz = honest_gz
    if suite == "tampered-index":
        # The signature below covers `honest`; the mirror serves `tampered`, which
        # has the SAME length and a different digest — so only the SHA-256 binding
        # can catch it (a length check would not, and the signature is valid).
        tampered = attacker_index(honest)
        assert len(tampered) == len(honest), "tamper must be length-preserving"
        with open(os.path.join(idx_dir, "Packages"), "wb") as f:
            f.write(tampered)
        # No `Packages.gz` for this suite: the release for it is not written
        # either, so the plain variant is the only candidate apt may use.
    else:
        with open(os.path.join(idx_dir, "Packages.gz"), "wb") as f:
            f.write(served_gz)

    rel_dir = os.path.join(REPO, "dists", suite)
    os.makedirs(rel_dir, exist_ok=True)
    release = suite_release(
        suite, honest, None if suite == "tampered-index" else honest_gz, date
    )
    with open(os.path.join(rel_dir, "Release"), "wb") as f:
        f.write(release)
    sign_release(rel_dir, release, signer)


def build_repo() -> None:
    pgp.self_test()  # never sign with a signer that fails the RFC 8032 vectors
    elf = build_linux_elf(HELLO_MSG)
    data_tar_gz = make_tar_gz([{"name": "usr/bin/hello-pagh", "mode": 0o755, "data": elf}])
    control_text = (
        "Package: hello-pagh\n"
        "Version: 1.0\n"
        "Architecture: amd64\n"
        "Maintainer: Pagh-OS <root@pagh>\n"
        "Description: tiny hello binary for the apt end-to-end test\n"
    ).encode()
    control_tar_gz = make_tar_gz([{"name": "control", "mode": 0o644, "data": control_text}])
    deb = build_deb(control_tar_gz, data_tar_gz)

    # --- second package: links (issue #18) -------------------------------------
    # A file, a relative symlink, an absolute symlink, a hard link sharing the
    # file's inode, and a member path longer than the 100-byte `name` field (GNU
    # `'L'`). `real` must come first: the hard link names it as its target.
    links_payload = b"links payload\n"
    long_leaf = "usr/share/links-pagh/" + ("l" * 110) + "/leaf.txt"
    links_data_tar_gz = make_tar_gz(
        fmt=tarfile.GNU_FORMAT,
        members=[
            {"name": "usr/share/links-pagh/real", "mode": 0o644, "data": links_payload},
            {"name": "usr/share/links-pagh/rel", "type": tarfile.SYMTYPE, "linkname": "real"},
            {
                "name": "usr/share/links-pagh/abs",
                "type": tarfile.SYMTYPE,
                "linkname": "/usr/share/links-pagh/real",
            },
            {
                "name": "usr/share/links-pagh/hard",
                "type": tarfile.LNKTYPE,
                "linkname": "usr/share/links-pagh/real",
            },
            {"name": long_leaf, "mode": 0o644, "data": b"long\n"},
        ],
    )
    links_control = (
        "Package: links-pagh\n"
        "Version: 1.0\n"
        "Architecture: amd64\n"
        "Maintainer: Pagh-OS <root@pagh>\n"
        "Description: symlink and hardlink fixture for issue #18\n"
    ).encode()
    links_deb = build_deb(
        make_tar_gz([{"name": "control", "mode": 0o644, "data": links_control}]),
        links_data_tar_gz,
    )

    stale = stale_deb()
    for suite, signer in SUITES:
        payload = stale if suite == "stale" else deb
        # Only the release suite carries the second package: the verification
        # suites stay minimal, so a refusal has exactly one candidate cause.
        links = links_deb if suite == "stable" else None
        write_suite(suite, signer, payload, tamper_deb=(suite == "tampered-deb"), links=links)

    print(f"built repo at {REPO}")
    print(f"  ELF         : usr/bin/hello-pagh ({len(elf)} bytes), prints {HELLO_MSG!r}")
    print(f"  .deb        : {len(deb)} bytes, sha256 {hashlib.sha256(deb).hexdigest()[:12]}…")
    print(
        f"  links .deb  : {len(links_deb)} bytes — symlink rel/abs, hard link, "
        f"{len('usr/share/links-pagh/' + 'l' * 110 + '/leaf.txt')}-byte member path"
    )
    for suite, signer in SUITES:
        print(f"  dists/{suite:15s} signed-by={signer or 'NONE (deliberately unsigned)'}")


def build_big_index(n: int) -> None:
    """DIAGNOSTIC (Pagh-OS apt parse-stage crash repro): generate a LARGE synthetic
    `Packages` index of `n` stanzas and write both `Packages` and `Packages.gz`
    under dists/stable/main/binary-amd64, so the kernel `apt update` streaming
    parse path is exercised at real-`main` scale over local HTTP (no live CDN).

    Mirrors the host-tests `bigindex` generator field-for-field (Package, Version,
    Architecture, Filename, Depends with version constraints + an OR group,
    Provides, Maintainer, a continuation-line Description, Size). This produces a
    multi-MiB decompressed index so the at-scale parse-stage fault reproduces
    deterministically.
    """
    parts = []
    for i in range(n):
        pkg = f"pkg-{i:06d}"
        parts.append(f"Package: {pkg}\n")
        parts.append(f"Version: {i % 10}.{i % 100}.{i % 7}-{i % 3}\n")
        parts.append("Architecture: amd64\n")
        parts.append(f"Filename: pool/main/p/{pkg}/{pkg}_{i % 10}.{i % 100}_amd64.deb\n")
        if i > 2:
            parts.append(
                f"Depends: pkg-{i-1:06d} (>= 1.0), pkg-{i-2:06d} | pkg-{i-3:06d} (>= 2.0)\n"
            )
        if i % 5 == 0:
            parts.append(f"Provides: virtual-{i:06d}, feature-x\n")
        parts.append("Maintainer: Pagh-OS <root@pagh>\n")
        parts.append(f"Description: synthetic package {pkg}\n")
        parts.append(" This is a continuation line describing the package in detail\n")
        parts.append(" across multiple physical lines for realism.\n")
        parts.append(f"Size: {1000 + i * 37}\n")
        parts.append("\n")
    packages = "".join(parts).encode()

    idx_dir = os.path.join(REPO, "dists", "stable", "main", "binary-amd64")
    os.makedirs(idx_dir, exist_ok=True)
    with open(os.path.join(idx_dir, "Packages"), "wb") as f:
        f.write(packages)
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:
        gz.write(packages)
    gz_bytes = out.getvalue()
    with open(os.path.join(idx_dir, "Packages.gz"), "wb") as f:
        f.write(gz_bytes)

    rel_dir = os.path.join(REPO, "dists", "stable")
    os.makedirs(rel_dir, exist_ok=True)
    release = suite_release("stable", packages, gz_bytes)
    with open(os.path.join(rel_dir, "Release"), "wb") as f:
        f.write(release)
    sign_release(rel_dir, release, "test")

    print(f"built BIG index at {idx_dir}")
    print(f"  stanzas     : {n}")
    print(f"  Packages    : {len(packages)} bytes ({len(packages)//1024} KiB)")
    print(f"  Packages.gz : {len(gz_bytes)} bytes ({len(gz_bytes)//1024} KiB)")


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

    # DIAGNOSTIC mode: `python3 mini_repo.py bigindex [N] [port]` builds a LARGE
    # synthetic Packages.gz (default 60000 stanzas) and serves it over local HTTP
    # so the kernel apt-update parse-stage crash reproduces without the live CDN.
    if mode == "bigindex":
        n = int(sys.argv[2]) if len(sys.argv) > 2 else 60000
        build_big_index(n)
        port = int(sys.argv[3]) if len(sys.argv) > 3 else 8000
        serve(port)
        return

    build_repo()
    if mode == "serve":
        port = int(sys.argv[2]) if len(sys.argv) > 2 else 8000
        serve(port)


if __name__ == "__main__":
    main()
