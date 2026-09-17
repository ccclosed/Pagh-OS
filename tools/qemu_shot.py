#!/usr/bin/env python3
"""Drive a running (or freshly booted) pagh QEMU instance through its monitor.

Two things the e2e scripts cannot do, both needed to *see* what the kernel draws
and to run the in-QEMU selftests without a human at the keyboard:

  * ``screendump`` — save the guest framebuffer (the pagh console, the status bar,
    the ``paint`` window, the mouse cursor) as a PNG.  Works headless, so it needs
    no display server: QEMU still emulates the VGA device and renders into its
    surface.
  * ``sendkey`` — type into the guest.  The pagh shell reads the **PS/2
    keyboard**, not the serial port, so this is the only way to answer the
    first-boot ``Y/n`` question or to run ``selftest`` from a script.

Examples
--------
Boot a fresh instance, answer the base-package question with ``n``, wait for the
shell prompt and screenshot it::

    python tools/qemu_shot.py --boot --answer-n --out /tmp/pagh-shell.png

Type a command and capture the result::

    python tools/qemu_shot.py --boot --answer-n --keys "selftest\\n" \\
        --settle 60 --out /tmp/pagh-selftest.png

Reuse an instance that is already running (any QEMU started with a monitor
socket, e.g. ``-monitor unix:/tmp/pagh_mon.sock,server,nowait``)::

    python tools/qemu_shot.py --monitor /tmp/pagh_mon.sock --keys "help\\n" --out /tmp/pagh-help.png

The screenshot is a normal PNG; open it, or read it with an image-capable tool.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
MONITOR_DEFAULT = "/tmp/pagh_mon.sock"
SERIAL_DEFAULT = "/tmp/pagh_serial.log"
PROMPT_MARKER = "pagh:"  # shell prompt: `pagh:<cwd>> ` (serial + framebuffer)
# The provisioning question itself is printed to the framebuffer only
# (`fb_println!`), so the serial log carries the *waiting* line instead — that is
# the marker a headless driver has to key off.
YN_WAIT_MARKER = "waiting for the Y/n answer"

# Characters the pagh shell accepts that are not spelled like their QEMU key
# name. Everything else is either a bare letter/digit or shift-<name>.
KEY_NAMES = {
    " ": "spc",
    "\n": "ret",
    "\r": "ret",
    "\t": "tab",
    "-": "minus",
    "=": "equal",
    "/": "slash",
    "\\": "backslash",
    ".": "dot",
    ",": "comma",
    ";": "semicolon",
    "'": "apostrophe",
    "[": "bracket_left",
    "]": "bracket_right",
    "`": "grave_accent",
    "!": "shift-1",
    "@": "shift-2",
    "#": "shift-3",
    "$": "shift-4",
    "%": "shift-5",
    "^": "shift-6",
    "&": "shift-7",
    "*": "shift-8",
    "(": "shift-9",
    ")": "shift-0",
    "_": "shift-minus",
    "+": "shift-equal",
    ":": "shift-semicolon",
    '"': "shift-apostrophe",
    "<": "shift-comma",
    ">": "shift-dot",
    "?": "shift-slash",
    "|": "shift-backslash",
    "~": "shift-grave_accent",
}


def key_name(ch: str) -> str:
    """Map one character to its QEMU ``sendkey`` name."""
    if ch in KEY_NAMES:
        return KEY_NAMES[ch]
    if ch.isalpha():
        return ch.lower() if ch.islower() else f"shift-{ch.lower()}"
    if ch.isdigit():
        return ch
    raise SystemExit(f"error: no key mapping for {ch!r} (extend KEY_NAMES)")


class Monitor:
    """Minimal QEMU monitor client (unix socket, or ``tcp:host:port``)."""

    def __init__(self, target: str, timeout: float = 15.0):
        self.sock = self._connect(target, timeout)
        time.sleep(0.3)
        self._drain()

    @staticmethod
    def _connect(target: str, timeout: float) -> socket.socket:
        """Open the monitor socket.

        A path (``/tmp/pagh_mon.sock``) is a unix socket, anything that names a
        host and port — ``host:port`` or ``tcp:host:port`` — is TCP. Both are
        what QEMU's ``-monitor`` accepts.
        """
        spec = target[4:] if target.startswith("tcp:") else target
        if ":" in spec and not spec.startswith("/"):
            host, _, port = spec.rpartition(":")
            if not host or not port.isdigit():
                raise SystemExit(f"error: --monitor wants a socket path or host:port, got {target!r}")
            family, address = socket.AF_INET, (host, int(port))
        else:
            family, address = socket.AF_UNIX, spec
        deadline = time.time() + timeout
        while True:
            sock = socket.socket(family, socket.SOCK_STREAM)
            try:
                sock.connect(address)
                return sock
            except OSError:
                sock.close()
                if time.time() > deadline:
                    raise SystemExit(
                        f"error: cannot connect to QEMU monitor {target!r} "
                        "(is QEMU running with -monitor unix:…,server,nowait?)"
                    )
                time.sleep(0.2)

    def _drain(self) -> str:
        self.sock.settimeout(0.4)
        out = b""
        try:
            while True:
                chunk = self.sock.recv(65536)
                if not chunk:
                    break
                out += chunk
        except OSError:
            pass
        return out.decode("utf-8", "replace")

    def command(self, cmd: str) -> str:
        """Send one monitor command and return whatever it echoed back."""
        self.sock.sendall(f"{cmd}\n".encode())
        time.sleep(0.15)
        return self._drain()

    @staticmethod
    def _check(reply: str, cmd: str) -> None:
        """Fail loudly on a monitor error instead of timing out later."""
        lowered = reply.lower()
        if "error" in lowered or "unknown command" in lowered or "invalid" in lowered:
            raise SystemExit(f"error: QEMU rejected {cmd!r}: {reply.strip()}")

    def send_keys(self, text: str, delay: float = 0.12) -> None:
        for ch in text:
            cmd = f"sendkey {key_name(ch)}"
            self._check(self.command(cmd), cmd)
            time.sleep(delay)

    def screendump(self, path: pathlib.Path) -> None:
        """Capture the guest framebuffer to `path` (PNG).

        Writes to a temporary file and renames it into place: an existing
        `path` is only replaced by a dump that actually succeeded, so a rejected
        `screendump` can no longer destroy the file it was asked to write.
        """
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.qemu_shot.tmp")
        if tmp.exists():
            tmp.unlink()
        reply = self.command(f"screendump {tmp} -f png")
        self._check(reply, "screendump")
        deadline = time.time() + 20
        while time.time() < deadline:
            if tmp.exists() and tmp.stat().st_size > 0:
                time.sleep(0.2)  # let QEMU finish flushing the file
                tmp.replace(path)
                return
            time.sleep(0.2)
        raise SystemExit(f"error: screendump produced no file at {path}")

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def resolve_ovmf(requested: str) -> pathlib.Path:
    """OVMF firmware, resolved by the same rules `tools/build.py` uses.

    `build.py` owns the search order (repo root, then the system copies); this
    delegates instead of keeping a second, drifting copy of the list.
    """
    sys.path.insert(0, str(ROOT / "tools"))
    import build  # noqa: PLC0415  (tools/build.py; guarded by __main__)

    return build.resolve_ovmf(requested)


def decode_keys(text: str) -> str:
    """Interpret the escapes a shell leaves alone in ``--keys``.

    ``--keys "selftest\\n"`` arrives as a literal backslash + ``n`` (bash does not
    expand it inside double quotes), so the two common ones are decoded here.
    """
    return text.replace("\\n", "\n").replace("\\r", "\r").replace("\\t", "\t")


def wait_for(path: pathlib.Path, marker: str, timeout: float) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            try:
                if marker in path.read_text(errors="replace"):
                    return True
            except OSError:
                pass
        time.sleep(0.5)
    return False


def boot(args: argparse.Namespace) -> tuple[subprocess.Popen, Monitor]:
    """Start QEMU headless with a monitor socket and a serial log."""
    iso_root = ROOT / "iso_root"
    if not (iso_root / "pagh.elf").exists():
        raise SystemExit("error: iso_root/pagh.elf missing — run `python tools/build.py stage --release` first")
    disk = pathlib.Path(args.disk)
    if not disk.is_absolute():
        disk = ROOT / disk
    if not disk.exists():
        # The kernel formats a blank device on boot, which is intended for a
        # freshly created image — say so, because `--disk` defaults to the repo's
        # own disk.img and a typo here is a disk image, not a file path.
        print(f"note: creating a blank 64M disk image at {disk}", file=sys.stderr)
        subprocess.run(["qemu-img", "create", "-f", "raw", str(disk), "64M"], check=True)

    monitor_target = args.monitor
    if not monitor_target.startswith("/") and ":" not in monitor_target:
        raise SystemExit("error: --boot needs a unix socket path (e.g. /tmp/pagh_mon.sock) or host:port")
    if monitor_target.startswith("/"):
        if os.path.exists(monitor_target):
            os.unlink(monitor_target)
        monitor_spec = f"unix:{monitor_target},server,nowait"
    else:
        monitor_spec = f"tcp:{monitor_target},server,nowait"
    serial = pathlib.Path(args.serial)
    if serial.exists():
        serial.unlink()

    cmd = [
        "qemu-system-x86_64",
        "-bios", str(resolve_ovmf(args.ovmf)),
        "-cpu", args.cpu,
        "-drive", f"file=fat:rw:{iso_root},format=raw",
        "-drive", f"file={disk},format=raw,if=none,id=hd0",
        "-device", "virtio-blk-pci,drive=hd0",
        "-netdev", "user,id=net0",
        "-device", "e1000,netdev=net0",
        "-m", args.memory,
        "-serial", f"file:{serial}",
        "-display", "none",
        "-no-reboot",
        "-monitor", monitor_spec,
    ]
    print("+", " ".join(cmd), file=sys.stderr)
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        mon = Monitor(monitor_target)
    except BaseException:
        # Never leave a booted guest behind because the monitor did not come up.
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        raise
    return proc, mon


def main() -> int:
    p = argparse.ArgumentParser(description="Screenshot / type into a pagh QEMU instance")
    p.add_argument("--boot", action="store_true", help="start QEMU headless first (needs a unix --monitor socket)")
    p.add_argument("--monitor", default=MONITOR_DEFAULT, help=f"QEMU monitor: unix socket path or host:port (default {MONITOR_DEFAULT})")
    p.add_argument("--serial", default=SERIAL_DEFAULT, help="serial log path (used with --boot / --wait-prompt)")
    p.add_argument("--out", default="/tmp/pagh_shot.png", help="screenshot path (PNG; replaced only by a successful dump)")
    p.add_argument("--keys", default="", help="text to type before the screenshot, e.g. 'selftest\\n'")
    p.add_argument("--answer-n", action="store_true", help="answer the first-boot base-package Y/n question with 'n'")
    p.add_argument("--wait-prompt", type=float, default=90.0, help="seconds to wait for the shell prompt (0 = don't)")
    p.add_argument("--settle", type=float, default=2.0, help="seconds to wait after typing, before the screenshot")
    p.add_argument("--keep-running", action="store_true", help="leave a --boot instance running after the screenshot")
    p.add_argument("--disk", default="disk.img", help="data disk image for --boot; created blank (64M) when absent, and the kernel formats a blank disk on boot")
    p.add_argument("--ovmf", default="OVMF.fd", help="OVMF firmware path (falls back to the system copy)")
    p.add_argument("--cpu", default="max", help="QEMU CPU model ('max' exposes RDSEED/RDRAND for the TLS path)")
    p.add_argument("--memory", default="1024M", help="guest RAM")
    args = p.parse_args()

    proc = None
    try:
        if args.boot:
            proc, mon = boot(args)
        else:
            mon = Monitor(args.monitor)

        serial = pathlib.Path(args.serial)
        if args.answer_n:
            # The first boot on a disk without python3 asks Y/n on the console and
            # waits for a keystroke before the shell starts.
            if wait_for(serial, YN_WAIT_MARKER, args.wait_prompt):
                print("answering the base-package question with 'n'", file=sys.stderr)
                mon.send_keys("n\n")
            else:
                print("warning: the Y/n question was not seen; continuing without an answer",
                      file=sys.stderr)

        if args.wait_prompt and not wait_for(serial, PROMPT_MARKER, args.wait_prompt):
            print(f"warning: shell prompt not seen in {args.serial} within {args.wait_prompt:.0f}s",
                  file=sys.stderr)

        if args.keys:
            keys = decode_keys(args.keys)
            print(f"typing {keys!r}", file=sys.stderr)
            mon.send_keys(keys)
        if args.settle:
            time.sleep(args.settle)

        out = pathlib.Path(args.out)
        mon.screendump(out)
        print(out)
        mon.close()
    finally:
        if proc is not None and not args.keep_running:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
