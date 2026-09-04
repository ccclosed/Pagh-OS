#!/usr/bin/env python3
"""Version-agnostic Limine loader locator/installer for pagh.

Resolves a usable ``BOOTX64.EFI`` without any version pin:

1. ``LIMINE_EFI`` env var (exact file path),
2. ``LIMINE_DIR`` env var (directory containing BOOTX64.EFI),
3. any ``limine*/BOOTX64.EFI`` tree in the repo root (any Limine version),
4. system install locations (``/usr/share/limine``, ``/usr/local/share/limine``),
5. otherwise: download the binary release from
   ``https://github.com/Limine-Bootloader/Limine/releases/latest/download/limine-binary.zip``
   (a stable, version-independent URL) and unpack it into ``limine/``.

An explicit version can still be requested with ``--version`` (or the
``LIMINE_VERSION`` env var) for reproducible setups, but nothing here is
hard-pinned.

Usage:
    python tools/limine.py                # ensure + print the loader path
    python tools/limine.py --dir limine/  # install destination override
    python tools/limine.py --version 12.7.0

The resolved BOOTX64.EFI path goes to stdout (so batch/PowerShell callers
can capture it); all progress chatter goes to stderr.
"""
from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import sys
import tempfile
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
RELEASES = "https://github.com/Limine-Bootloader/Limine/releases"
# System fallbacks (Linux distros ship the loader via a `limine` package).
SYSTEM_DIRS = ["/usr/share/limine", "/usr/local/share/limine"]
USER_AGENT = "pagh-os-build (+https://github.com/Limine-Bootloader/Limine)"


def _log(msg: str) -> None:
    print(msg, file=sys.stderr)


def find_loader(root: pathlib.Path = ROOT) -> pathlib.Path | None:
    """Return an existing BOOTX64.EFI path, or None. Never downloads."""
    env_efi = os.environ.get("LIMINE_EFI")
    if env_efi and pathlib.Path(env_efi).is_file():
        return pathlib.Path(env_efi)
    env_dir = os.environ.get("LIMINE_DIR")
    if env_dir:
        cand = pathlib.Path(env_dir) / "BOOTX64.EFI"
        if cand.is_file():
            return cand
    # Any locally unpacked Limine tree, regardless of version suffix.
    local = sorted(root.glob("limine*/BOOTX64.EFI"), key=lambda p: p.stat().st_mtime)
    if local:
        return local[-1]
    for sysdir in SYSTEM_DIRS:
        cand = pathlib.Path(sysdir) / "BOOTX64.EFI"
        if cand.is_file():
            return cand
    return None


def download(version: str | None, dest: pathlib.Path) -> None:
    """Fetch limine-binary.zip for `version` (None = latest) to `dest`.

    Tries, in order: urllib (honors HTTPS_PROXY), curl.exe/schannel
    (different TLS stack), and PowerShell Invoke-WebRequest. Raises
    SystemExit with manual instructions when every transport fails.
    """
    if version:
        tag = version.lstrip("v")
        url = f"{RELEASES}/download/v{tag}/limine-binary.zip"
    else:
        url = f"{RELEASES}/latest/download/limine-binary.zip"
    _log(f"==> Downloading Limine loader: {url}")
    attempts: list[tuple[str, object]] = [("urllib", lambda: _dl_urllib(url, dest))]
    if shutil.which("curl") or os.name == "nt":  # Windows 10+ ships curl.exe
        attempts.append(("curl", lambda: _dl_curl(url, dest)))
    if os.name == "nt":
        attempts.append(("powershell", lambda: _dl_powershell(url, dest)))
    for name, attempt in attempts:
        try:
            attempt()
            if dest.is_file() and dest.stat().st_size > 0:
                _log(f"==> Downloaded via {name}")
                return
            _log(f"==> download via {name} produced an empty file; retrying")
        except SystemExit:
            raise
        except Exception as exc:  # noqa: BLE001 - report every transport failure
            _log(f"==> download via {name} failed: {exc}")
    raise SystemExit(
        "error: could not download limine-binary.zip automatically.\n"
        f"       Download it manually from {RELEASES}/latest, then either:\n"
        "         - unpack it into ./limine/ (BOOTX64.EFI at the top level), or\n"
        "         - set LIMINE_EFI to an existing BOOTX64.EFI path.\n"
        "       (If a corporate proxy is in use, set HTTPS_PROXY and retry.)"
    )


def _dl_urllib(url: str, dest: pathlib.Path) -> None:
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=120) as resp, open(dest, "wb") as fh:
        shutil.copyfileobj(resp, fh)


def _dl_curl(url: str, dest: pathlib.Path) -> None:
    subprocess.run(["curl", "-sSL", "--retry", "2", "-o", str(dest), url], check=True)


def _dl_powershell(url: str, dest: pathlib.Path) -> None:
    subprocess.run(["powershell", "-NoProfile", "-Command",
                    "[Net.ServicePointManager]::SecurityProtocol = "
                    "[Net.SecurityProtocolType]::Tls12; "
                    f"Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile '{dest}'"],
                   check=True)


def install(root: pathlib.Path = ROOT, version: str | None = None,
            destdir: pathlib.Path | None = None) -> pathlib.Path:
    """Download and unpack a Limine binary release; return BOOTX64.EFI."""
    dest = destdir or (root / "limine")
    with tempfile.TemporaryDirectory(prefix="limine-dl-") as tmp:
        tmpdir = pathlib.Path(tmp)
        archive = tmpdir / "limine-binary.zip"
        download(version, archive)
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(tmpdir / "unpack")
        # limine-binary.zip nests everything under limine-binary/; normalize
        # by flattening every file to the top level of the destination.
        dest.mkdir(parents=True, exist_ok=True)
        for item in (tmpdir / "unpack").iterdir():
            files = item.rglob("*") if item.is_dir() else [item]
            for f in files:
                if not f.is_file():
                    continue
                target = dest / (f.relative_to(item) if item.is_dir() else f.name)
                target.parent.mkdir(parents=True, exist_ok=True)
                if not target.exists():
                    shutil.copy2(f, target)
    loader = dest / "BOOTX64.EFI"
    if not loader.is_file():
        raise SystemExit(f"error: limine-binary.zip did not contain BOOTX64.EFI (installed into {dest})")
    _log(f"==> Limine loader installed: {loader}")
    return loader


def ensure(root: pathlib.Path = ROOT, destdir: pathlib.Path | None = None) -> pathlib.Path:
    """Find or install the loader; always return a usable BOOTX64.EFI path."""
    found = find_loader(root)
    if found:
        return found
    if destdir and (destdir / "BOOTX64.EFI").is_file():
        return destdir / "BOOTX64.EFI"
    _log("==> No Limine loader found locally (any limine*/ tree); installing.")
    return install(root, os.environ.get("LIMINE_VERSION") or None, destdir)


def main() -> None:
    p = argparse.ArgumentParser(description="Locate or install the Limine BOOTX64.EFI loader")
    p.add_argument("--version", default=os.environ.get("LIMINE_VERSION"),
                   help="pin a specific Limine release (default: latest)")
    p.add_argument("--dir", default=os.environ.get("LIMINE_DIR"),
                   help="directory to search/install into (default: auto / limine/)")
    a = p.parse_args()
    destdir = pathlib.Path(a.dir) if a.dir else None
    loader = ensure(ROOT, destdir)
    # The single stdout line is the contract for script callers.
    print(loader)


if __name__ == "__main__":
    main()
