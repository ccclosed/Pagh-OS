#!/usr/bin/env python3
"""Live E2E proof for F-x86n-1: the installer must refuse a symlink-escape `.deb`.

Boots a real Pagh-OS guest under QEMU (`tools/e2e.py shell`) against a local HTTP
server, downloads the crafted packages from `tests/pkg/craft_evil_deb.py` with the
guest's own unauthenticated `pkg` downloader, and asserts on the serial log:

  * `evil-autorun.deb` (`y -> /etc` + `y/autorun`) is refused with a clear
    `refused unsafe install path` error naming `/mnt/y/autorun`, and
    `cat /mnt/etc/autorun` answers **not found** — the boot-time root payload never
    lands;
  * `evil-symlink.deb` (`x -> /` + `x/pwned_root`) is refused and
    `/mnt/pwned_root` does not exist;
  * `evil-relative.deb` (`z -> ../../..` + `z/escape`) is refused;
  * the plain `..` control (`evil-dotdot.deb`) is still skipped, not installed;
  * the negative marker strings (`PWN-AUTORUN-VIA-PKG`, `PWN-SYMLINK-ESCAPE`,
    `PWN-RELATIVE-ESCAPE`, `PWN-DOTDOT`) appear nowhere in the serial log;
  * **positive control**: `normal-hello.deb` (two files + one absolute symlink with
    no member below it) installs (`pkg: installed 3 files under /mnt`), its binary
    reads back, and the installed symlink is followed.

Run from the repository root:

    python3 tests/pkg/e2e_pkg_escape.py

Exit status: 0 = all evidence present; 1 = the guest/behaviour failed; 2 = setup
problem (no QEMU/port busy/...), never reported as a security failure.

The `.deb` files are generated into `--out` (default `/tmp/pagh-pkg-poc`), not
committed: `craft_evil_deb.py` is the reproducible source.
"""
from __future__ import annotations

import argparse
import http.server
import pathlib
import re
import socket
import subprocess
import sys
import threading

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
E2E = ROOT / "tools" / "e2e.py"
DEFAULT_SERIAL = ROOT / ".cache" / "pkg_escape_serial.log"

EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_SETUP = 2

#: Markers that must never reach the guest tree / serial log.
PWN_MARKERS = (
    "PWN-AUTORUN-VIA-PKG",
    "PWN-SYMLINK-ESCAPE",
    "PWN-RELATIVE-ESCAPE",
    "PWN-DOTDOT",
)


def log(msg: str) -> None:
    print(f"[pkg-e2e] {msg}", flush=True)


def port_is_free(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(("0.0.0.0", port))
        except OSError:
            return False
    return True


def start_server(directory: pathlib.Path, port: int):
    """Serve `directory` on all interfaces (the guest reaches the host as 10.0.2.2)."""

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(directory), **kwargs)

        def log_message(self, fmt, *args):  # keep the driver output readable
            log(f"http {fmt % args}")

    server = http.server.ThreadingHTTPServer(("0.0.0.0", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def build_script(port: int) -> str:
    """The `tools/e2e.py shell --script` interaction: one command per package."""
    p = f"pkg 10.0.2.2"
    lines = [
        "# F-x86n-1 live proof: crafted .debs served over HTTP from the host.",
        f"cmd {p} /evil-autorun.deb {port}",
        "wait pkg: install failed: refused unsafe install path 120",
        "cmd cat /mnt/etc/autorun",
        "wait cat: /mnt/etc/autorun: not found 30",
        f"cmd {p} /evil-symlink.deb {port}",
        "wait pkg: install failed: refused unsafe install path 120",
        "cmd cat /mnt/pwned_root",
        "wait cat: /mnt/pwned_root: not found 30",
        f"cmd {p} /evil-relative.deb {port}",
        "wait pkg: install failed: refused unsafe install path 120",
        # Plain `..` control: member skipped, package completes (0 members).
        f"cmd {p} /evil-dotdot.deb {port}",
        "wait pkg: installed 0 files under /mnt 60",
        # Positive control: a normal package still installs, links included.
        f"cmd {p} /normal-hello.deb {port}",
        "wait pkg: done 120",
        "cmd cat /mnt/usr/bin/hello-pkg",
        "wait hello from normal-hello 30",
        "cmd ls -l /mnt/usr/bin",
        "wait hello-alias -> /usr/bin/hello-pkg 30",
        "sleep 2",
    ]
    return "\n".join(lines) + "\n"


def run_e2e(script: pathlib.Path, serial: pathlib.Path) -> int:
    cmd = [
        sys.executable,
        str(E2E),
        "shell",
        "--script",
        str(script),
        "--serial-log",
        str(serial),
        "--boot-timeout",
        "300",
        "--settle",
        "8",
        "--timeout",
        "240",
    ]
    log("running: " + " ".join(str(c) for c in cmd))
    return subprocess.call(cmd, cwd=str(ROOT))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8080, help="host HTTP port")
    parser.add_argument(
        "--out",
        default="/tmp/pagh-pkg-poc",
        help="where to generate the crafted .debs",
    )
    parser.add_argument(
        "--serial-log", default=str(DEFAULT_SERIAL), help="serial capture path"
    )
    parser.add_argument(
        "--keep-script",
        action="store_true",
        help="keep the generated e2e interaction script",
    )
    args = parser.parse_args()

    # 1. Reproducible hostile packages.
    sys.path.insert(0, str(HERE))
    import craft_evil_deb  # noqa: E402  (local test fixture beside this driver)

    out = pathlib.Path(args.out)
    made = craft_evil_deb.build_all(out)
    log(f"crafted {len(made)} packages in {out}")

    # 2. HTTP server on the host peer address the guest uses (10.0.2.2).
    if not port_is_free(args.port):
        print(
            f"[pkg-e2e] SETUP: port {args.port} is busy; re-run with --port N",
            file=sys.stderr,
        )
        return EXIT_SETUP
    server = start_server(out, args.port)
    log(f"serving {out} on 0.0.0.0:{args.port} (guest: 10.0.2.2)")

    # 3. Drive the real guest through the qemu monitor.
    script = pathlib.Path(args.out) / "pkg_escape_e2e.script"
    script.write_text(build_script(args.port), encoding="utf-8")
    serial = pathlib.Path(args.serial_log)
    serial.parent.mkdir(parents=True, exist_ok=True)
    if serial.exists():
        serial.unlink()
    try:
        rc = run_e2e(script, serial)
    finally:
        server.shutdown()
        server.server_close()
        if not args.keep_script:
            script.unlink(missing_ok=True)

    if not serial.is_file():
        print("[pkg-e2e] FAIL: no serial log was produced", file=sys.stderr)
        return EXIT_FAIL
    text = serial.read_text(encoding="utf-8", errors="replace")

    # 4. Decisive negative: the payload bytes must not have reached the guest.
    leaked = [m for m in PWN_MARKERS if m in text]
    refused = re.findall(
        r"pkg: install failed: refused unsafe install path '([^']+)'", text
    )
    checks = [
        (
            "evil-autorun refused at /mnt/y/autorun",
            "/mnt/y/autorun" in refused,
        ),
        (
            "evil-symlink refused at /mnt/x/pwned_root",
            "/mnt/x/pwned_root" in refused,
        ),
        (
            "evil-relative refused at /mnt/z/escape",
            "/mnt/z/escape" in refused,
        ),
        (
            "/mnt/etc/autorun does not exist",
            "cat: /mnt/etc/autorun: not found" in text,
        ),
        ("/mnt/pwned_root does not exist", "cat: /mnt/pwned_root: not found" in text),
        (
            "plain .. control installed nothing",
            "pkg: installed 0 files under /mnt" in text,
        ),
        (
            "normal package installs",
            "pkg: installed 3 files under /mnt" in text,
        ),
        ("normal package binary reads back", "hello from normal-hello" in text),
        (
            "normal package absolute symlink stored verbatim",
            "hello-alias -> /usr/bin/hello-pkg" in text,
        ),
        ("no PWN marker in the serial log", not leaked),
    ]
    ok = rc == 0
    evidence = [
        line
        for line in text.splitlines()
        if any(
            key in line
            for key in (
                "pkg: install failed",
                "pkg: installed",
                "pkg: done",
                "cat: /mnt/etc/autorun",
                "cat: /mnt/pwned_root",
                "hello from normal-hello",
                "hello-alias",
            )
        )
    ]
    print()
    print("== serial evidence ==")
    for line in evidence[-40:]:
        print(f"  {line.strip()}")
    print()
    print("== checks ==")
    for label, passed in checks:
        print(f"  [{'PASS' if passed else 'FAIL'}] {label}")
        ok = ok and passed
    if leaked:
        print(f"  leaked markers: {leaked}")
    print(f"\nserial log: {serial}")
    print(f"crafted packages: {out}")
    print(f"verdict: {'PASS' if ok else 'FAIL'}")
    return EXIT_PASS if ok else EXIT_FAIL


if __name__ == "__main__":
    raise SystemExit(main())