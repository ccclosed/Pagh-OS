#!/usr/bin/env python3
"""In-guest E2E driver for Pagh-OS — QEMU + serial to an exit code (Linux path).

The three Windows-only PowerShell harnesses (`tools/e2e_*.ps1`) require `pwsh`,
which is not installed in the Linux dev/CI environment, so the in-guest checks
(`LXSELFTEST` markers, the local apt scenario, the big-index repro) never ran.
This driver is the portable replacement and the single entry point for every
in-guest verification the project does:

    tools/e2e_local_mirror.ps1   ->  python3 tools/e2e.py local-mirror
    tools/e2e_live_update.ps1    ->  python3 tools/e2e.py live-update
    tools/e2e_bigindex.ps1       ->  python3 tools/e2e.py bigindex [--in-ram] [--expect-crash]
    tools/smoke_assertions.ps1   ->  python3 tools/e2e.py smoke
    (new)                        ->  python3 tools/e2e.py selftest
    (new, generic)               ->  python3 tools/e2e.py shell --cmd '...' --expect '...'

BOOT IDENTITY (why a green run is trustworthy): `elf_sha256` is the hash of the
BUILT artifact, which by itself proves nothing about what the guest executed — an
old kernel booted from another device prints the same regression markers. Every
run therefore (a) uses a private per-run NVRAM copy, so firmware boot state never
survives a run, and (b) patches a fixed-length `[BOOTID]` placeholder inside its
staged ELF copy with a per-run tag and requires that tag back on serial. A
mismatch is a refusal (exit 2), a kernel without the marker is reported as
`unverified` — never as green. See `tools/e2e_boot_integrity.md`.

WHAT EVERY RUN DOES: build the kernel with the mode's required cargo features
(`tools/build.py`, CI parity: `--locked` + rust-lld link) -> stage `iso_root`
(kernel ELF + `BOOTX64.EFI` via `tools/limine.py` + `boot/limine.conf`) -> boot
under QEMU with `-serial file:<log>` -> wait for the mode's serial markers ->
print the evidence and return 0 on success / non-zero on failure.

HOW THE GUEST IS DRIVEN ("feeding the serial script"): the kernel shell reads
**PS/2 scancodes only** (`src/shell/mod.rs::try_read_scancode` ->
`drivers::ps2_kbd`); its serial port is transmit-only (`src/drivers/serial.rs`),
so keystrokes cannot be pushed through `-serial stdio`. Input is therefore typed
into the guest through the QEMU monitor (`-monitor unix:<sock>`, `sendkey`),
exactly like a human at the keyboard, while the guest's own serial output is read
back from the `-serial file:` log. This is what makes the in-guest `selftest`
shell command scriptable.

THE DEFAULT ARTIFACTS ARE NEVER LOST: `iso_root/pagh.elf` (and the linked
`target/<triple>/<profile>/pagh.elf`) are backed up before staging and restored
after the run, in a `finally` block — including on Ctrl-C/SIGTERM, and on the
next run if a previous one was killed with SIGKILL (stale-backup self-heal).
A shared `flock` on `.cache/e2e.lock` keeps two concurrent harness runs from
clobbering `iso_root/` or `disk.img`; the mini-repo tree is snapshotted and
restored too (the big-index mode rewrites tracked files under `tools/mini_repo/`).

Evidence for issue-closing: the full serial log (`serial_*.log`), the filtered
evidence lines printed to stdout, and a machine-readable summary JSON
(`.cache/e2e_<mode>_summary.json`: verdict, checks, ELF sha256, git HEAD).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time

TOOLS = pathlib.Path(__file__).resolve().parent
ROOT = TOOLS.parent
TARGET = "x86_64-unknown-none"
CACHE = ROOT / ".cache"
BACKUP_DIR = CACHE / "e2e_backup"
BACKUP_MANIFEST = BACKUP_DIR / "manifest.json"
MINI_REPO = TOOLS / "mini_repo"
MINI_REPO_BACKUP = CACHE / "e2e_mini_repo_backup"

# `tools/build.py` is the project's single source of truth for the cargo + rust-lld
# invocation (CI parity), so the driver imports it instead of re-implementing it.
sys.path.insert(0, str(TOOLS))
import build as build_driver  # noqa: E402
import limine  # noqa: E402

EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_SETUP = 2
# Distinct from EXIT_SETUP: the run itself was fine, but boot identity could not
# be proven (kernel without the `[BOOTID]` half). Never reported as green.
EXIT_UNVERIFIED = 3

# Regression scale floor for the `bigindex` mode (issue #17). The parse-stage
# `[EXC #14]` crash was observed at roughly 5 459 stanzas / ~4 MiB, so anything
# materially smaller cannot exercise the failing path and a green verdict from it
# is a false negative rather than evidence. `--allow-small-index` overrides the
# refusal for deliberate sub-scale runs (negative controls), and such a run must
# never be quoted as #17 proof.
BIGINDEX_MIN_STANZAS = 50_000

# OVMF: a combined image works with `-bios`; the split firmware the distros ship
# is used as a pflash pair (CODE readonly + a writable VARS copy).
SYSTEM_OVMF_CODE = (
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    "/usr/share/edk2/x64/OVMF_CODE.fd",
    "/usr/share/ovmf/OVMF_CODE.fd",
)
SYSTEM_OVMF_VARS = (
    "/usr/share/OVMF/OVMF_VARS.fd",
    "/usr/share/edk2/ovmf/OVMF_VARS.fd",
    "/usr/share/edk2/x64/OVMF_VARS.fd",
    "/usr/share/ovmf/OVMF_VARS.fd",
)

MODE_DEFAULTS = {
    "selftest": {"timeout": 300.0, "serial": "serial_selftest.log"},
    "local-mirror": {"timeout": 240.0, "serial": "serial_e2e.log"},
    "live-update": {"timeout": 1200.0, "serial": "serial_live.log"},
    "bigindex": {"timeout": 300.0, "serial": "serial_bigindex.log"},
    "shell": {"timeout": 180.0, "serial": "serial_shell.log"},
}

# Features a mode cannot work without; `--features` only ADDS to this list.
MODE_REQUIRED_FEATURES = {
    "local-mirror": ["lx_selftest"],
    "live-update": ["lx_livetest"],
    "bigindex": ["lx_bigindex"],
}

# Default evidence filters per mode (the `[WATCHDOG]`/`[DIAG]`/`LXSELFTEST`/`[EXC #N]`
# marker family AGENTS.md pins the E2E surface to).
MODE_EVIDENCE = {
    "selftest": (
        r"SELFTEST SUMMARY", r"=== kernel self-test", r"^FAIL", r"^ok\s",
        r"Self-test complete", r"Running kernel self-test", r"pagh:/>",
    ),
    "local-mirror": (
        r"apt:|LXSELFTEST apt_e2e|hello from apt|LXSELFTEST https_get|Package_Fetcher\(tls\)",
    ),
    "live-update": (
        r"apt:|LIVE_APT_UPDATE|LXSELFTEST live_update|Resident_Index_Footprint|net::tls|busybox",
    ),
    "bigindex": (
        r"apt:|BIGINDEX|LXSELFTEST bigindex|EXC #\d+|PAGE FAULT|RIP=",
    ),
    "shell": (
        r"SELFTEST|LXSELFTEST|EXC #|FAIL|PASS|hello|apt:|pagh:/>",
    ),
}

# ── boot-identity markers (t1 integrity, issue-independent infra) ─────────────
#
# PROBLEM: nothing proved that the guest executed the ELF this driver built.
# `elf_sha256` in the summary is the hash of the BUILT artifact, and an old
# kernel booted from the data disk's own ESP would print the same LXSELFTEST
# markers — a green verdict on a foreign image is indistinguishable from success.
#
# FIX (two layers):
#   1. prevention — per-run NVRAM (see resolve_ovmf): firmware boot state cannot
#      leak from one run into the next;
#   2. proof — the kernel prints `[BOOTID] <tag>` at boot; the driver overwrites
#      that fixed-length placeholder inside its staged ELF copy with a per-run
#      tag and requires the tag back on serial. The tag exists only in the staged
#      bytes, so a stale kernel cannot produce it.
#
# `[BOOTID]` is part of the serial regression surface (AGENTS.md Conventions):
# never rename or reflow it. The kernel half is `tools/e2e_bootid_kernel.patch`.
BOOTID_MARKER = "[BOOTID]"
BOOT_TAG_PREFIX = b"PAGH-BOOTID-"
BOOT_TAG_PLACEHOLDER = BOOT_TAG_PREFIX + b"0" * 16  # fixed length, patched in place
BOOT_TAG_LEN = len(BOOT_TAG_PLACEHOLDER)

PROVISION_MARK = "provision: waiting for the Y/n answer"
SHELL_MARK = "Type 'help' for available commands"
SHELL_PROMPT = "pagh:/>"


# ───────────────────────────── small helpers ─────────────────────────────


def log(msg: str) -> None:
    print(f"[e2e] {msg}", flush=True)


def warn(msg: str) -> None:
    print(f"[e2e] WARNING: {msg}", flush=True)


class SetupError(Exception):
    """Bad environment / bad invocation -> exit 2 (no verification happened)."""


class HarnessError(Exception):
    """The harness could not reach a verdict -> exit 1."""


class BootIdentityError(HarnessError):
    """The guest did not provably execute the staged image -> exit 2 (refused).

    A run whose boot identity is unknown cannot support a verdict: a stale kernel
    prints the same regression markers, so a green result would be unfalsifiable."""



def sha256_file(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def git_head() -> str:
    try:
        out = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                             capture_output=True, text=True, timeout=30)
        return out.stdout.strip() if out.returncode == 0 else "unknown"
    except Exception:
        return "unknown"


def run_checked(cmd: list, what: str, timeout: float | None = None) -> subprocess.CompletedProcess:
    log("$ " + " ".join(str(c) for c in cmd))
    proc = subprocess.run([str(c) for c in cmd], cwd=ROOT, capture_output=True,
                          text=True, timeout=timeout)
    if proc.stdout.strip():
        print(proc.stdout.rstrip(), flush=True)
    if proc.stderr.strip():
        print(proc.stderr.rstrip(), file=sys.stderr, flush=True)
    if proc.returncode != 0:
        raise HarnessError(f"{what} failed (exit {proc.returncode})")
    return proc


def read_text(path: pathlib.Path) -> str:
    try:
        return path.read_bytes().decode("utf-8", "replace")
    except (FileNotFoundError, IsADirectoryError):
        return ""


def tail(text: str, lines: int = 25) -> str:
    return "\n".join(text.splitlines()[-lines:])


def regex_first(pattern: str, text: str) -> str | None:
    m = re.search(pattern, text)
    return m.group(0).strip() if m else None


def rel(path: pathlib.Path) -> str:
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


# ───────────────────────── artifacts (backup/restore) ─────────────────────────


class Artifacts:
    """Backs up the paths a run overwrites and restores them afterwards.

    `iso_root/pagh.elf` is the project's default boot artifact: a harness that
    stages a feature ELF there MUST put the original back, otherwise the next
    `./run.sh` silently boots a test kernel. The backup also survives a hard kill
    (SIGKILL): the manifest is written before staging, and the next run restores
    any stale backup before taking its own."""

    def __init__(self, keep: bool) -> None:
        self.keep = keep
        self.files: list[tuple[pathlib.Path, pathlib.Path]] = []
        self.active = False

    def stale_manifest(self) -> dict | None:
        try:
            return json.loads(BACKUP_MANIFEST.read_text(encoding="utf-8"))
        except (FileNotFoundError, ValueError):
            return None

    def heal_stale(self) -> None:
        """Restore a backup left behind by a previous, hard-killed run."""
        data = self.stale_manifest()
        if not data:
            return
        restored = []
        for entry in data.get("files", []):
            orig, bak = pathlib.Path(entry["orig"]), pathlib.Path(entry["backup"])
            if bak.is_file():
                orig.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(bak, orig)
                restored.append(orig)
        if restored:
            log("restored artifacts left behind by an earlier interrupted run: "
                + ", ".join(rel(p) for p in restored))
        shutil.rmtree(BACKUP_DIR, ignore_errors=True)

    def backup(self, paths: list[pathlib.Path]) -> None:
        shutil.rmtree(BACKUP_DIR, ignore_errors=True)
        BACKUP_DIR.mkdir(parents=True, exist_ok=True)
        entries = []
        for orig in paths:
            if not orig.is_file():
                continue
            bak = BACKUP_DIR / hashlib.sha256(str(orig).encode()).hexdigest()[:16]
            shutil.copy2(orig, bak)
            entries.append({"orig": str(orig), "backup": str(bak),
                            "sha256": sha256_file(orig)})
            self.files.append((orig, bak))
        BACKUP_MANIFEST.write_text(json.dumps({"files": entries}, indent=2),
                                   encoding="utf-8")
        self.active = True
        log("backed up " + (", ".join(rel(p) for p in paths if p.is_file())
                            or "nothing (no prior artifacts)"))

    def restore(self) -> list[str]:
        if not self.active:
            return []
        done = []
        for orig, bak in self.files:
            if not bak.is_file():
                continue
            orig.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(bak, orig)
            done.append(rel(orig))
        shutil.rmtree(BACKUP_DIR, ignore_errors=True)
        self.active = False
        return done


class TreeSnapshot:
    """Whole-directory snapshot/restore (the big-index mode rewrites the tracked
    `tools/mini_repo/` index files, which must not linger as a dirty worktree)."""

    def __init__(self, src: pathlib.Path, dst: pathlib.Path) -> None:
        self.src, self.dst, self.taken = src, dst, False

    def take(self) -> None:
        if not self.src.is_dir():
            return
        shutil.rmtree(self.dst, ignore_errors=True)
        shutil.copytree(self.src, self.dst)
        self.taken = True

    def restore(self) -> None:
        if not self.taken or not self.dst.is_dir():
            return
        shutil.rmtree(self.src, ignore_errors=True)
        shutil.copytree(self.dst, self.src)
        shutil.rmtree(self.dst, ignore_errors=True)
        self.taken = False


# RATIONALE for the default path: the lock must cover runs in *different*
# worktrees, because they share real host resources — the mirror port (8000) and
# the QEMU user-net forward — even though each worktree has its own `iso_root/`
# and `.cache/`. A per-repo `.cache/e2e.lock` cannot see the other worktree.
DEFAULT_LOCK_PATH = pathlib.Path(os.environ.get("PAGH_E2E_LOCK", "/tmp/pagh-e2e.lock"))


class RunLock:
    """Serializes harness runs across worktrees (mirror port + QEMU user-net)."""

    def __init__(self, path: pathlib.Path | None = None, timeout: float = 3600.0) -> None:
        self.path = pathlib.Path(path) if path else DEFAULT_LOCK_PATH
        self.timeout = timeout
        self.fh = None

    def __enter__(self) -> "RunLock":
        self.path.parent.mkdir(parents=True, exist_ok=True)
        try:
            import fcntl
        except ImportError:  # non-POSIX host: no locking available
            return self
        self.fh = open(self.path, "w")
        deadline = time.time() + self.timeout
        warned = False
        while True:
            try:
                fcntl.flock(self.fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.fh.write(str(os.getpid()))
                self.fh.flush()
                return self
            except OSError:
                if not warned:
                    log(f"another e2e.py run holds {self.path} (any worktree); waiting for it ...")
                    warned = True
                if time.time() > deadline:
                    raise SetupError(f"timed out waiting for {self.path}")
                time.sleep(1.0)

    def __exit__(self, *exc) -> None:
        if self.fh is not None:
            try:
                import fcntl
                fcntl.flock(self.fh, fcntl.LOCK_UN)
            except Exception:
                pass
            self.fh.close()
            self.fh = None


# ───────────────────────── QEMU: firmware, disk, guest ─────────────────────────


def resolve_ovmf(requested: str, vars_dest: pathlib.Path) -> tuple[str, list[str]]:
    """Return (description, qemu args) for the firmware, with PER-RUN NVRAM.

    Resolution order matches the repo's other drivers: an explicit `--ovmf`/`$OVMF`
    or a repo-root `OVMF.fd` is a combined image (`-bios`); otherwise the split
    system `OVMF_CODE.fd` + `OVMF_VARS.fd` pair is used as pflash.

    WHY PER-RUN VARS: NVRAM (boot entries, BootOrder) is guest-writable state. A
    shared `.cache/OVMF_VARS.fd` carried entries from one run into the next, so a
    later run could boot a *different device* than the freshly staged ESP — with
    the driver still reporting the staged ELF's hash. `vars_dest` is therefore a
    private per-run copy, always rewritten from the pristine template:
      * `$PAGH_E2E_VARS_TEMPLATE` (test hook: lets a synthetic test feed a
        deliberately poisoned NVRAM and prove the driver does not trust it);
      * else the pristine system `OVMF_VARS.fd`;
      * else the legacy shared `.cache/OVMF_VARS.fd` (fallback only — it may hold
        accumulated NVRAM, hence the warning).
    """
    if requested:
        cand = pathlib.Path(requested)
        if not cand.is_absolute():
            cand = ROOT / cand
        if cand.is_file():
            return f"combined {cand}", ["-bios", str(cand)]
        if requested != "OVMF.fd":
            raise SetupError(f"OVMF not found: {cand}")
    code = next((pathlib.Path(p) for p in SYSTEM_OVMF_CODE if pathlib.Path(p).is_file()), None)
    if code is None:
        raise SetupError(
            "OVMF firmware not found: put OVMF.fd in the repo root, set $OVMF, "
            "or install edk2-ovmf / ovmf (system OVMF_CODE.fd + OVMF_VARS.fd)")
    env_tpl = os.environ.get("PAGH_E2E_VARS_TEMPLATE")
    fallback_tpl = CACHE / "OVMF_VARS.fd"
    if env_tpl:
        vars_tpl = pathlib.Path(env_tpl)
        if not vars_tpl.is_file():
            raise SetupError(f"PAGH_E2E_VARS_TEMPLATE={env_tpl} does not exist")
        log(f"NVRAM template override (test hook): {vars_tpl}")
    else:
        vars_tpl = next((pathlib.Path(p) for p in SYSTEM_OVMF_VARS if pathlib.Path(p).is_file()), None)
        if vars_tpl is None and fallback_tpl.is_file():
            warn(f"no system OVMF_VARS.fd template; reusing {rel(fallback_tpl)} as a template "
                 f"(it may carry NVRAM boot entries accumulated by earlier runs)")
            vars_tpl = fallback_tpl
    if vars_tpl is None:
        raise SetupError("OVMF_CODE.fd found but no system OVMF_VARS.fd template")
    vars_dest.parent.mkdir(parents=True, exist_ok=True)
    # Always a fresh copy: NVRAM must never survive a run, in either direction.
    shutil.copy2(vars_tpl, vars_dest)
    return (f"pflash {code} + {vars_dest} (per-run NVRAM from {vars_tpl})",
            ["-drive", f"if=pflash,format=raw,readonly=on,file={code}",
             "-drive", f"if=pflash,format=raw,file={vars_dest}"])


def prepare_disk(explicit: str | None, reuse_scratch: bool) -> pathlib.Path:
    """Return the data disk image to hand to QEMU.

    Default: a per-run scratch COPY of the repo's `disk.img`, so a harness can
    write to `/mnt` (apt install, fs demo) without mutating the developer's disk
    and every run starts from the same state. `--disk PATH` uses that image as-is
    (it will be modified by the guest)."""
    qemu_img = shutil.which("qemu-img")
    if explicit:
        path = pathlib.Path(explicit)
        if not path.is_absolute():
            path = ROOT / path
        if not path.is_file():
            if not qemu_img:
                raise SetupError(f"disk image {path} missing and qemu-img unavailable")
            run_checked([qemu_img, "create", "-f", "raw", str(path), "64M"], "qemu-img create")
        return path
    scratch = CACHE / "e2e_disk.img"
    CACHE.mkdir(parents=True, exist_ok=True)
    if not reuse_scratch or not scratch.is_file():
        source = ROOT / "disk.img"
        if source.is_file():
            shutil.copy2(source, scratch)
            log("disk: per-run scratch copy of disk.img -> .cache/e2e_disk.img")
        else:
            if not qemu_img:
                raise SetupError("no disk.img and qemu-img unavailable")
            run_checked([qemu_img, "create", "-f", "raw", str(scratch), "64M"],
                        "qemu-img create")
            log("disk: fresh 64 MiB scratch image (the kernel formats it at boot)")
    return scratch


def _elf_load_ranges(data: bytes) -> list[tuple[int, int]]:
    """File-offset range of every PT_LOAD segment of an ELF64 LSB image.

    WHY: a byte pattern can occur in a section the loader never copies into
    memory (`.symtab`, `.debug_str`, `.comment`). Patching such an occurrence
    would leave the running kernel printing the *unpatched* constant — a false
    refusal. The tag is only useful if the patched bytes are inside a segment
    Limine actually maps.
    """
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise HarnessError("staged kernel is not an ELF64 little-endian image")
    e_phoff = struct.unpack_from("<Q", data, 0x20)[0]
    e_phentsize = struct.unpack_from("<H", data, 0x36)[0]
    e_phnum = struct.unpack_from("<H", data, 0x38)[0]
    if not (e_phoff and e_phentsize and e_phnum):
        raise HarnessError("staged kernel has no program header table")
    ranges: list[tuple[int, int]] = []
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        if off + 56 > len(data):
            raise HarnessError("truncated program header table in staged kernel")
        if struct.unpack_from("<I", data, off)[0] != 1:  # PT_LOAD only
            continue
        p_offset = struct.unpack_from("<Q", data, off + 0x08)[0]
        p_filesz = struct.unpack_from("<Q", data, off + 0x20)[0]
        ranges.append((p_offset, p_offset + p_filesz))
    return ranges


def make_boot_tag(elf_sha256: str) -> bytes:
    """Per-run tag: fixed length, build hash + pid, unique per run."""
    tail = f"{elf_sha256[:12]}{os.getpid() % 10000:04d}"
    tag = BOOT_TAG_PREFIX + tail.encode()
    assert len(tag) == BOOT_TAG_LEN, (len(tag), BOOT_TAG_LEN)
    return tag


def patch_boot_tag(elf_path: pathlib.Path, tag: bytes) -> dict:
    """Overwrite the fixed-length BOOTID placeholder inside the STAGED ELF copy.

    Only the staged copy is touched (the build artifact keeps its hash). Returns
    an audit record: every occurrence seen, which one was inside a PT_LOAD
    segment, sha256 before/after, byte diff count, size before/after. Callers
    treat `patched=False` as "identity unprovable" (warn), never as success.
    """
    info: dict = {"marker": BOOTID_MARKER, "tag": tag.decode(), "patched": False,
                  "reason": "", "occurrences": 0, "load_occurrences": 0}
    data = elf_path.read_bytes()
    offsets, start = [], 0
    while True:
        i = data.find(BOOT_TAG_PLACEHOLDER, start)
        if i < 0:
            break
        offsets.append(i)
        start = i + 1
    info["occurrences"] = len(offsets)
    if len(tag) != BOOT_TAG_LEN:
        raise HarnessError(f"boot tag has wrong length: {len(tag)} != {BOOT_TAG_LEN}")
    if not offsets:
        info["reason"] = ("placeholder not found in the staged ELF: this kernel predates the "
                          f"{BOOTID_MARKER} marker (apply tools/e2e_bootid_kernel.patch)")
        return info
    loads = _elf_load_ranges(data)
    loaded = [o for o in offsets if any(lo <= o and o + BOOT_TAG_LEN <= hi for lo, hi in loads)]
    info["load_occurrences"] = len(loaded)
    info["offsets"] = offsets
    info["load_ranges"] = loads
    if len(loaded) != 1:
        info["reason"] = (f"expected exactly one placeholder inside a PT_LOAD segment, "
                          f"found {len(loaded)} of {len(offsets)} occurrence(s)")
        return info
    off = loaded[0]
    patched = data[:off] + tag + data[off + BOOT_TAG_LEN:]
    if len(patched) != len(data):
        raise HarnessError("boot-tag patch changed the image size (must be length-preserving)")
    diff = [i for i in range(len(data)) if data[i] != patched[i]]
    # The tag shares its `PAGH-BOOTID-` prefix with the placeholder, so the number
    # of differing bytes is 1..BOOT_TAG_LEN (16 when the pid tail has no zeros).
    if not diff or any(not (off <= i < off + BOOT_TAG_LEN) for i in diff) or len(diff) > BOOT_TAG_LEN:
        raise HarnessError(
            f"boot-tag patch touched unexpected bytes ({len(diff)} differing, outside "
            f"[{off}, {off + BOOT_TAG_LEN}))")
    elf_path.write_bytes(patched)
    info.update(patched=True, offset=off, diff_bytes=len(diff),
                size_before=len(data), size_after=len(patched),
                sha256_before=hashlib.sha256(data).hexdigest(),
                sha256_after=hashlib.sha256(patched).hexdigest())
    return info


def boot_integrity(serial_text: str, tag: str | None) -> dict:
    """Decide whether the guest executed the staged image (strict, no heuristics).

    Levels: `verified` (the per-run tag came back), `fail` (a different image or
    no image ran), `unverifiable` (the staged kernel has no BOOTID support yet —
    the caller warns and records it rather than pretending to know).

    WHY no firmware/device heuristic: QEMU's `-drive file=fat:rw:<stage>` shows up
    in OVMF's BDS log as "UEFI QEMU HARDDISK" too, so `BdsDxe: starting ... HARDDISK`
    is printed by *successful* boots as well — such a check would be a false alarm.
    """
    seen = re.findall(re.escape(BOOTID_MARKER) + r" (\S+)", serial_text)
    banner = bool(re.search(r"\[INFO\] serial\b", serial_text)) or "Welcome to pagh OS Shell!" in serial_text
    res = {"marker": BOOTID_MARKER, "tag_expected": tag, "tags_seen": seen[:8],
           "kernel_banner_seen": banner, "verified": False, "level": "unverifiable", "detail": ""}
    if not tag:
        res["detail"] = ("staged image carries no BOOTID placeholder: boot identity "
                         "cannot be proven (kernel half missing)")
        return res
    if not banner:
        res.update(level="fail",
                   detail="no kernel boot banner on serial — the guest never ran our kernel")
        return res
    if tag in seen:
        res.update(verified=True, level="verified",
                   detail=f"guest printed the staged image tag {tag}")
        return res
    if seen:
        res.update(level="fail",
                   detail=f"guest printed tag(s) {seen} but the staged image tag is {tag}: "
                          f"a DIFFERENT image was booted")
        return res
    res.update(level="fail",
               detail=f"staged image tag {tag} never appeared on serial: the guest did not "
                      f"execute the staged image")
    return res


def stage_iso_root(elf: pathlib.Path, stage_dir: pathlib.Path, limine_dir: str) -> None:
    """Rebuild the boot tree: kernel ELF + BOOTX64.EFI + limine.conf (in both
    places Limine looks)."""
    loader = limine.ensure(ROOT, pathlib.Path(limine_dir) if limine_dir else None)
    if not loader.is_file():
        raise HarnessError("Limine loader unavailable (run: python3 tools/limine.py)")
    shutil.rmtree(stage_dir, ignore_errors=True)
    (stage_dir / "EFI" / "BOOT").mkdir(parents=True)
    shutil.copy2(elf, stage_dir / "pagh.elf")
    shutil.copy2(loader, stage_dir / "EFI" / "BOOT" / "BOOTX64.EFI")
    conf = (ROOT / "boot" / "limine.conf").read_text(encoding="utf-8")
    (stage_dir / "limine.conf").write_text(conf, encoding="utf-8")
    (stage_dir / "EFI" / "BOOT" / "limine.conf").write_text(conf, encoding="utf-8")
    log(f"staged {rel(stage_dir)}/pagh.elf ({elf.stat().st_size} bytes) "
        f"+ {loader} -> EFI/BOOT/BOOTX64.EFI")
    return stage_dir


class Guest:
    """A QEMU instance with serial -> file and a monitor socket for keyboard input."""

    def __init__(self, argv: list[str], serial_log: pathlib.Path,
                 monitor: pathlib.Path, stderr_log: pathlib.Path) -> None:
        self.argv = argv
        self.serial_log = serial_log
        self.monitor_path = monitor
        self.stderr_log = stderr_log
        self.proc: subprocess.Popen | None = None
        self._stderr_fh = None

    def start(self) -> None:
        for p in (self.serial_log, self.monitor_path):
            try:
                p.unlink()
            except FileNotFoundError:
                pass
        self.stderr_log.parent.mkdir(parents=True, exist_ok=True)
        self._stderr_fh = open(self.stderr_log, "wb")
        log("$ " + " ".join(self.argv))
        self.proc = subprocess.Popen(self.argv, cwd=ROOT, stdout=subprocess.DEVNULL,
                                     stderr=self._stderr_fh)

    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def returncode(self) -> int | None:
        return None if self.proc is None else self.proc.poll()

    def serial(self) -> str:
        return read_text(self.serial_log)

    def stop(self, grace: float = 8.0) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                try:
                    self.proc.wait(timeout=grace)
                except subprocess.TimeoutExpired:
                    warn("QEMU did not die after SIGKILL")
        if self._stderr_fh is not None:
            self._stderr_fh.close()
            self._stderr_fh = None
        try:
            self.monitor_path.unlink()
        except FileNotFoundError:
            pass


# ───────────────────────── keyboard input through the monitor ─────────────────────────

# QEMU `sendkey` names. The kernel's PS/2 decoder is set-1, and QEMU translates
# these names to the same scancodes a real keyboard would send.
_KEY_SYMBOLS = {
    " ": "spc", "!": "shift-1", '"': "shift-apostrophe", "#": "shift-3",
    "$": "shift-4", "%": "shift-5", "&": "shift-7", "'": "apostrophe",
    "(": "shift-9", ")": "shift-0", "*": "shift-8", "+": "shift-equal",
    ",": "comma", "-": "minus", ".": "dot", "/": "slash", ":": "shift-semicolon",
    ";": "semicolon", "<": "shift-comma", "=": "equal", ">": "shift-dot",
    "?": "shift-slash", "@": "shift-2", "[": "bracket_left", "\\": "backslash",
    "]": "bracket_right", "^": "shift-6", "_": "shift-minus", "`": "grave_accent",
    "{": "shift-bracket_left", "|": "shift-backslash", "}": "shift-bracket_right",
    "~": "shift-grave_accent", "\n": "ret", "\t": "tab",
}


def key_name(ch: str) -> str:
    if ch.isascii() and ch.isalpha():
        return ch.lower() if ch.islower() else "shift-" + ch.lower()
    if ch.isdigit():
        return ch
    if ch in _KEY_SYMBOLS:
        return _KEY_SYMBOLS[ch]
    raise HarnessError(f"cannot type {ch!r} through the PS/2 keyboard map")


class Keyboard:
    """Types text into the guest via the QEMU monitor `sendkey` command."""

    def __init__(self, monitor_path: pathlib.Path, key_delay: float = 0.15) -> None:
        self.path = monitor_path
        self.key_delay = key_delay
        self.sock: socket.socket | None = None

    def connect(self, timeout: float = 120.0) -> None:
        deadline = time.time() + timeout
        last: Exception | None = None
        while time.time() < deadline:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(5.0)
            try:
                s.connect(str(self.path))
            except OSError as exc:
                last = exc
                s.close()
                time.sleep(0.5)
                continue
            self.sock = s
            self._drain()
            return
        raise HarnessError(f"QEMU monitor socket unreachable ({self.path}): {last}")

    def _drain(self, wait: float = 0.05) -> str:
        assert self.sock is not None
        out = b""
        self.sock.settimeout(wait)
        try:
            while True:
                chunk = self.sock.recv(65536)
                if not chunk:
                    break
                out += chunk
                self.sock.settimeout(0.02)
        except (socket.timeout, TimeoutError):
            pass
        self.sock.settimeout(5.0)
        return out.decode("utf-8", "replace")

    def cmd(self, line: str) -> str:
        if self.sock is None:
            raise HarnessError("QEMU monitor not connected")
        try:
            self.sock.sendall((line + "\n").encode())
        except OSError as exc:
            raise HarnessError(f"QEMU monitor send failed: {exc}")
        time.sleep(0.02)
        return self._drain()

    def key(self, name: str, delay: float | None = None) -> None:
        self.cmd(f"sendkey {name}")
        time.sleep(self.key_delay if delay is None else delay)

    def text(self, text: str) -> None:
        for ch in text:
            self.key(key_name(ch))

    def close(self) -> None:
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None


# ───────────────────────── serial waiting ─────────────────────────


def wait_for(guest: Guest, patterns, timeout: float, *, tick: float = 1.5,
             on_tick=None, label: str = "marker") -> str | None:
    """Poll the serial log until one of `patterns` matches (returns it) or the
    timeout expires (returns None). Fails fast when QEMU exits (a panic/abort
    stops the log growing, so waiting out the full timeout would just waste it)."""
    deadline = time.time() + timeout
    compiled = [re.compile(p) for p in patterns]
    while time.time() < deadline:
        text = guest.serial()
        for pat in compiled:
            if pat.search(text):
                return pat.pattern
        if guest.proc is not None and not guest.alive():
            time.sleep(1.0)  # let the last buffered bytes land
            text = guest.serial()
            for pat in compiled:
                if pat.search(text):
                    return pat.pattern
            raise HarnessError(
                f"QEMU exited (rc={guest.returncode()}) before the {label} appeared; "
                f"serial tail:\n{tail(text)}")
        if on_tick is not None:
            on_tick(text)
        time.sleep(tick)
    return None


def wait_for_shell(guest: Guest, kb: Keyboard, a, boot_timeout: float) -> bool:
    """Get to the interactive shell prompt: answer the first-boot provisioning
    question when it appears, then wait for the banner/prompt."""
    deadline = time.time() + boot_timeout
    answered = False
    while time.time() < deadline:
        text = guest.serial()
        if PROVISION_MARK in text and not answered:
            answered = True
            key = {"skip": "n", "yes": "y", "ask": "ret"}.get(a.provision, "ret")
            log(f"provisioning prompt seen; answering '{key}' "
                f"({'skip the python3 download' if key == 'n' else 'install in the background'})")
            kb.key(key)
        if SHELL_MARK in text or SHELL_PROMPT in text:
            return True
        if guest.proc is not None and not guest.alive():
            raise HarnessError(f"QEMU exited (rc={guest.returncode()}) before the shell "
                               f"prompt; serial tail:\n{tail(guest.serial())}")
        time.sleep(1.5)
    return False


# ───────────────────────── background mirror ─────────────────────────


def port_free(port: int) -> bool:
    s = socket.socket()
    s.settimeout(0.5)
    try:
        s.connect(("127.0.0.1", port))
        return False
    except OSError:
        return True
    finally:
        s.close()


class Mirror:
    """`tools/mini_repo.py serve|bigindex` as a child process, with an HTTP
    readiness probe (a fixed `sleep 2` is what made the PowerShell harnesses
    flaky)."""

    def __init__(self, args: list[str], port: int, out_log: pathlib.Path) -> None:
        self.args = args
        self.port = port
        self.out_log = out_log
        self.proc: subprocess.Popen | None = None
        self._fh = None

    def start(self, ready_path: str = "/dists/stable/main/binary-amd64/Packages.gz",
              timeout: float = 180.0) -> None:
        if not port_free(self.port):
            raise SetupError(
                f"TCP port {self.port} is already in use: another e2e run (possibly in a "
                f"different worktree — see {DEFAULT_LOCK_PATH}) or a stray "
                f"`mini_repo.py serve` holds it. Stop it and retry: the guest's in-kernel "
                f"URL is hard-coded to http://10.0.2.2:{self.port}")
        self.out_log.parent.mkdir(parents=True, exist_ok=True)
        self._fh = open(self.out_log, "wb")
        cmd = [sys.executable, str(TOOLS / "mini_repo.py")] + self.args
        log("$ " + " ".join(cmd))
        self.proc = subprocess.Popen(cmd, cwd=ROOT, stdout=self._fh, stderr=self._fh)
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise HarnessError(f"mini_repo.py exited ({self.proc.returncode}); log:\n"
                                   f"{read_text(self.out_log)[-2000:]}")
            try:
                s = socket.socket()
                s.settimeout(2.0)
                s.connect(("127.0.0.1", self.port))
                s.sendall(f"GET {ready_path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n".encode())
                data = s.recv(64)
                s.close()
                if b"200" in data.split(b"\r\n", 1)[0]:
                    log(f"mirror ready on 127.0.0.1:{self.port} ({ready_path})")
                    return
            except OSError:
                pass
            time.sleep(0.5)
        raise HarnessError(f"mirror did not become ready on port {self.port} within "
                           f"{timeout:.0f}s")

    def stop(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        if self._fh is not None:
            self._fh.close()
            self._fh = None


# ───────────────────────── the harness ─────────────────────────


class Harness:
    def __init__(self, a: argparse.Namespace, mode: str) -> None:
        self.a = a
        self.mode = mode
        d = MODE_DEFAULTS.get(mode, {"timeout": 300.0, "serial": f"serial_{mode}.log"})
        self.timeout = a.timeout if a.timeout is not None else d["timeout"]
        self.serial_log = pathlib.Path(a.serial_log) if a.serial_log else ROOT / d["serial"]
        if not self.serial_log.is_absolute():
            self.serial_log = ROOT / self.serial_log
        self.monitor = CACHE / f"e2e_{mode.replace('-', '_')}_monitor.sock"
        self.qemu_stderr = CACHE / f"e2e_{mode.replace('-', '_')}_qemu_stderr.log"
        self.qemu_log = ROOT / f"qemu_{mode.replace('-', '_')}_debug.log"
        self.mirror_out = CACHE / f"e2e_{mode.replace('-', '_')}_mirror.log"
        self.json_path = (pathlib.Path(a.json) if a.json
                          else CACHE / f"e2e_{mode.replace('-', '_')}_summary.json")
        self.stage_dir = pathlib.Path(a.stage_dir) if a.stage_dir else ROOT / "iso_root"
        if not self.stage_dir.is_absolute():
            self.stage_dir = ROOT / self.stage_dir
        self.features: list[str] = []
        self.elf: pathlib.Path | None = None
        self.elf_sha = ""
        self.guest: Guest | None = None
        self.kb: Keyboard | None = None
        self.mirror: Mirror | None = None
        self.artifacts = Artifacts(keep=a.keep_artifacts)
        self.mini_repo_snapshot = TreeSnapshot(MINI_REPO, MINI_REPO_BACKUP)
        # Per-run private NVRAM: firmware must never inherit boot state.
        self.run_dir = CACHE / f"e2e_run_{mode.replace('-', '_')}_{os.getpid()}"
        self.vars_path = self.run_dir / "OVMF_VARS.fd"
        self.tag: str | None = None
        self.boot_patch: dict | None = None
        self.boot_record: dict | None = None
        self.started_at = time.time()
        self.checks: list[dict] = []
        self.metrics: dict = {}
        self.extra_evidence: list[str] = list(a.extra_wait or [])

    # ── checks / report ──
    def check(self, label: str, ok: bool, evidence: str = "") -> bool:
        self.checks.append({"label": label, "status": "passed" if ok else "failed",
                            "evidence": evidence})
        colour = "\033[32m" if ok else "\033[31m"
        print(f"  {colour}{'PASS' if ok else 'FAIL'}\033[0m  {label}"
              + (f": {evidence}" if evidence else ""), flush=True)
        return ok

    def info_check(self, label: str, evidence: str) -> None:
        """A check that ran but could not be decided (e.g. pre-BOOTID kernel):
        recorded as `unverified` — never as green — so a summary reader sees it."""
        self.checks.append({"label": label, "status": "unverified", "evidence": evidence})
        print(f"  \033[33mWARN\033[0m  {label}: {evidence}", flush=True)

    def verify_boot(self) -> None:
        """Post-run boot-identity gate. Called before the report is written."""
        text = self.guest.serial() if self.guest else ""
        info = boot_integrity(text, self.tag)
        self.boot_record = info
        if info["level"] == "verified":
            self.check("guest booted the staged image (BOOTID)", True, info["detail"])
        elif info["level"] == "fail":
            raise BootIdentityError(info["detail"])
        else:
            msg = info["detail"]
            if self.a.require_boot_proof:
                raise BootIdentityError(msg + " (--require-boot-proof)")
            warn("BOOT IDENTITY UNVERIFIED: " + msg)
            self.info_check("guest booted the staged image (BOOTID)", msg + " (see docs)")

    def evidence_lines(self) -> list[str]:
        patterns = self.a.evidence_regex or list(MODE_EVIDENCE.get(self.mode, ()))
        if not patterns:
            return []
        rx = re.compile("|".join(f"(?:{p})" for p in patterns))
        seen, out = set(), []
        for line in self.guest.serial().splitlines() if self.guest else []:
            if rx.search(line) and line.strip() and line not in seen:
                seen.add(line)
                out.append(line.rstrip())
        return out

    def write_summary(self, verdict: str, exit_code: int) -> None:
        self.json_path.parent.mkdir(parents=True, exist_ok=True)
        summary = {
            "mode": self.mode,
            "verdict": verdict,
            "exit_code": exit_code,
            "features": self.features,
            "profile": "debug" if self.a.debug else "release",
            "timeout_s": self.timeout,
            "duration_s": round(time.time() - self.started_at, 1),
            "serial_log": str(self.serial_log),
            "qemu_returncode": self.guest.returncode() if self.guest else None,
            "elf": str(self.elf) if self.elf else None,
            "elf_sha256": self.elf_sha,
            "boot": self.boot_record,
            "boot_patch": self.boot_patch,
            "git_head": git_head(),
            "checks": self.checks,
            "metrics": self.metrics,
            "evidence": self.evidence_lines()[:400],
        }
        self.json_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")

    def print_report(self, verdict: str) -> None:
        print("\n================ SERIAL EVIDENCE (filtered) ================", flush=True)
        lines = self.evidence_lines()
        limit = self.a.evidence_limit
        for line in lines[:limit]:
            print(line)
        if len(lines) > limit:
            print(f"... ({len(lines) - limit} more lines; full log: {self.serial_log})")
        print("\n================ RESULT ================", flush=True)
        print(f"mode        : {self.mode}")
        print(f"verdict     : {verdict}")
        print(f"serial log  : {self.serial_log}")
        print(f"summary json: {self.json_path}")
        print(f"duration    : {time.time() - self.started_at:.1f} s")
        for key, value in self.metrics.items():
            print(f"{key:<11} : {value}")
        if self.boot_record is not None:
            print(f"boot identity: {self.boot_record['level']}"
                  + (f" (tag {self.boot_record['tag_expected']})"
                     if self.boot_record.get("tag_expected")
                     else " (no BOOTID support in the staged kernel)"))
            print(f"  {self.boot_record['detail']}")

    # ── lifecycle ──
    def build_and_stage(self) -> None:
        a = self.a
        required = list(MODE_REQUIRED_FEATURES.get(self.mode, []))
        if self.mode == "bigindex" and a.in_ram:
            required.append("lx_bigindex_inram")
        extra = [f.strip() for f in (a.features or "").split(",") if f.strip()]
        seen, self.features = set(), []
        for f in required + extra:
            if f not in seen:
                seen.add(f)
                self.features.append(f)
        log(f"building kernel ({'debug' if a.debug else 'release'}"
            + (f", features: {','.join(self.features)}" if self.features else "") + ")")
        self.elf = build_driver.build("debug" if a.debug else "release",
                                     ",".join(self.features))
        log(f"linked: {self.elf} ({self.elf.stat().st_size} bytes)")
        self.elf_sha = sha256_file(self.elf)
        log(f"elf sha256: {self.elf_sha}")
        # Back up what staging overwrites, BEFORE overwriting it.
        profile = "debug" if a.debug else "release"
        self.artifacts.backup([self.stage_dir / "pagh.elf",
                               ROOT / "target" / TARGET / profile / "pagh.elf"])
        stage_iso_root(self.elf, self.stage_dir, a.limine_dir)
        # Boot-identity: patch ONLY the staged copy, never the build artifact.
        tag = make_boot_tag(self.elf_sha)
        self.boot_patch = patch_boot_tag(self.stage_dir / "pagh.elf", tag)
        if self.boot_patch.get("patched"):
            self.tag = self.boot_patch["tag"]
            log(f"boot tag {self.tag} patched into the staged ELF at file offset "
                f"{self.boot_patch['offset']} (inside a PT_LOAD segment; "
                f"{self.boot_patch['diff_bytes']} bytes changed, size "
                f"{self.boot_patch['size_before']}->{self.boot_patch['size_after']}, "
                f"sha256 {self.boot_patch['sha256_before'][:16]}->"
                f"{self.boot_patch['sha256_after'][:16]})")
        else:
            warn("BOOT IDENTITY UNVERIFIED: " + self.boot_patch.get("reason", "no reason")
                 + " — a green verdict from this run does NOT prove which image the guest ran; "
                   "apply tools/e2e_bootid_kernel.patch and re-run")

    def boot(self, *, net: bool = True, need_keyboard: bool = False) -> None:
        a = self.a
        if not shutil.which("qemu-system-x86_64"):
            raise SetupError("qemu-system-x86_64 not found in PATH")
        disk = prepare_disk(a.disk, a.reuse_scratch_disk)
        self.run_dir.mkdir(parents=True, exist_ok=True)
        _, ovmf_args = resolve_ovmf(a.ovmf, self.vars_path)
        argv = ["qemu-system-x86_64", "-cpu", a.cpu, "-m", a.memory]
        argv += ovmf_args
        argv += ["-drive", f"file=fat:rw:{self.stage_dir},format=raw",
                 "-drive", f"file={disk},format=raw,if=none,id=hd0",
                 "-device", "virtio-blk-pci,drive=hd0"]
        if net:
            # QEMU user-net: the guest reaches a host-side mirror on 10.0.2.2:8000
            # and, for the live mode, the outside world.
            argv += ["-netdev", "user,id=net0", "-device", "e1000,netdev=net0"]
        if a.rtc:
            # Guest-visible wall clock. Needed for the negative TLS clock-gate case:
            # `--rtc base=2020-01-01` boots before the committed certificate's
            # validity window, which must make the fail-closed verifier refuse.
            argv += ["-rtc", a.rtc]
        argv += ["-serial", f"file:{self.serial_log}",
                 "-monitor", f"unix:{self.monitor},server,nowait",
                 "-display", "none", "-no-reboot",
                 "-d", "guest_errors", "-D", str(self.qemu_log)]
        self.guest = Guest(argv, self.serial_log, self.monitor, self.qemu_stderr)
        self.guest.start()
        if need_keyboard:
            self.kb = Keyboard(self.monitor, a.key_delay)
            self.kb.connect()

    def cleanup(self, keep: bool | None = None) -> None:
        keep = self.a.keep_artifacts if keep is None else keep
        if self.run_dir.is_dir():
            shutil.rmtree(self.run_dir, ignore_errors=True)  # NVRAM never survives a run
        if self.mirror is not None:
            self.mirror.stop()
        if self.kb is not None:
            self.kb.close()
        if self.guest is not None:
            self.guest.stop()
        self.mini_repo_snapshot.restore()
        if not keep:
            restored = self.artifacts.restore()
            if restored:
                log("restored default artifacts: " + ", ".join(restored))
        elif self.artifacts.active:
            warn("--keep-artifacts: the feature ELF is left in place; "
                 "run 'python3 tools/e2e.py restore' to put the default ELF back")

    def mirror_start(self, args: list[str], port: int) -> None:
        self.mirror = Mirror(args, port, self.mirror_out)
        self.mirror.start()


# ───────────────────────── modes ─────────────────────────


def mode_selftest(h: Harness) -> tuple[str, int]:
    """Run the in-guest `selftest` shell command and collect PASS/FAIL.

    This is the check the PowerShell harnesses never covered: the kernel's
    in-QEMU self-test suite (`src/test.rs`, 60+ routines) is only reachable from
    the interactive shell, and the suite's machine-readable verdict is
    `SELFTEST SUMMARY: <n> routines, <m> failed checks`."""
    a = h.a
    h.boot(need_keyboard=True)
    assert h.kb is not None and h.guest is not None
    if not wait_for_shell(h.guest, h.kb, a, a.boot_timeout):
        raise HarnessError(f"the shell prompt never appeared within {a.boot_timeout:.0f}s; "
                           f"serial tail:\n{tail(h.guest.serial())}")
    log("shell prompt reached; typing the selftest command")
    commands = a.cmd or ["selftest"]
    for cmd in commands:
        h.kb.text(cmd)
        h.kb.key("ret")
    pattern = wait_for(h.guest, [r"SELFTEST SUMMARY: (\d+) routines, (\d+) failed checks"],
                       h.timeout, label="selftest summary")
    text = h.guest.serial()
    summary = regex_first(r"SELFTEST SUMMARY: \d+ routines, \d+ failed checks", text)
    m = re.search(r"SELFTEST SUMMARY: (\d+) routines, (\d+) failed checks", text)
    routines = int(m.group(1)) if m else 0
    failed = int(m.group(2)) if m else -1
    h.metrics["routines"] = routines
    h.metrics["failed"] = failed
    h.metrics["ok lines"] = len(re.findall(r"^ok\s", text, re.M))
    ok = h.check("in-guest `selftest` produced a summary", bool(pattern or summary),
                 summary or "no SELFTEST SUMMARY line")
    ok &= h.check("0 failed checks", failed == 0, f"{failed} failed" if failed >= 0 else "unknown")
    extras = re.findall(r"^FAIL.*$", text, re.M)
    ok &= h.check("no FAIL lines on serial", not extras,
                  extras[0] if extras else "none")
    for pattern_extra in a.expect or []:
        found = re.search(pattern_extra, text)
        ok &= h.check(f"expected /{pattern_extra}/", bool(found),
                      found.group(0).strip() if found else "not found")
    # The in-kernel suite has its own summary, but an `lx_selftest` build runs a
    # SEPARATE LXSELFTEST harness whose failures only appear as
    # `[ERROR] LXSELFTEST <case> FAIL ...` lines. Without this check a run with a
    # failing LXSELFTEST case still reported PASS (observed in t10: `getcwd FAIL`
    # slipped through) — exactly the false-green class this harness must not emit.
    lx_fails = re.findall(r"LXSELFTEST \S+ FAIL[^\r\n]*", text)
    ok &= h.check("no LXSELFTEST case failures", not lx_fails,
                  lx_fails[0] if lx_fails else "none")
    for extra in h.extra_evidence:
        found = re.search(extra, text)
        h.check(f"extra marker /{extra}/ (best effort)", bool(found),
                found.group(0).strip() if found else "not observed")
    return ("PASS" if ok else "FAIL"), (EXIT_PASS if ok else EXIT_FAIL)


def mode_local_mirror(h: Harness) -> tuple[str, int]:
    """Deterministic local-mirror apt E2E (port of e2e_local_mirror.ps1)."""
    a = h.a
    if a.port != 8000:
        warn(f"--port {a.port}: the in-guest harness hard-codes http://10.0.2.2:8000, "
             f"so a non-default port can only work for a mirror on 8000")
    h.mini_repo_snapshot.take()
    run_checked([sys.executable, str(TOOLS / "mini_repo.py"), "build"], "mini_repo build")
    h.boot()
    assert h.guest is not None
    h.mirror_start(["serve", str(a.port)], a.port)
    answered = False

    def on_tick(text: str) -> None:
        nonlocal answered
        if not answered and PROVISION_MARK in text and h.kb is not None:
            answered = True
            h.kb.key("n")
        m = re.findall(r"LXSELFTEST apt_e2e: index loaded \((\d+) packages\)", text)
        if m:
            log(f"... index loaded ({m[-1]} packages)")

    if h.kb is None:  # local-mirror needs no prompt interaction, but a 'n' keeps the
        h.kb = Keyboard(h.monitor, a.key_delay)  # shell from starting a python download
        try:
            h.kb.connect()
        except HarnessError:
            h.kb = None
    found = wait_for(h.guest, [r"LXSELFTEST apt_e2e (?:PASS|FAIL)"], h.timeout,
                     on_tick=on_tick, label="apt_e2e verdict")
    if found:
        time.sleep(a.settle)  # let "hello from apt" land before teardown
    if h.kb is not None:
        h.kb.close()
    text = h.guest.serial()
    idx = re.search(r"LXSELFTEST apt_e2e: index loaded \((\d+) packages\)", text)
    h.metrics["index packages"] = int(idx.group(1)) if idx else "n/a"
    ok = h.check("apt_e2e verdict marker reached", bool(found),
                 regex_first(r"LXSELFTEST apt_e2e (?:PASS|FAIL)[^\r\n]*", text) or "timeout")
    ok &= h.check("index loaded (>0 packages)", bool(idx))
    get = regex_first(r"apt: Get http://10\.0\.2\.2(?::\d+)?/\S*Packages\S*", text)
    ok &= h.check("index fetch from the local mirror (R5.2)", bool(get), get or "")
    unpacked = regex_first(r"apt: \[\d+/\d+\] Unpacked \S+ \(\d+ files,", text)
    ok &= h.check("install wrote files onto ext2 /mnt", bool(unpacked), unpacked or "")
    ok &= h.check("apt_e2e PASS", bool(re.search(r"LXSELFTEST apt_e2e PASS[^\r\n]*", text)),
                  regex_first(r"LXSELFTEST apt_e2e PASS[^\r\n]*", text) or "no PASS line")
    ok &= h.check("installed static binary ran ('hello from apt')", "hello from apt" in text)
    ok &= h.check("no FAIL marker on serial",
                  not re.search(r"LXSELFTEST apt_e2e FAIL", text))
    for extra in h.extra_evidence:
        found_extra = re.search(extra, text)
        h.check(f"extra marker /{extra}/ (best effort)", bool(found_extra),
                found_extra.group(0).strip() if found_extra else "not observed")
    return ("PASS" if ok else "FAIL"), (EXIT_PASS if ok else EXIT_FAIL)


def mode_live_update(h: Harness) -> tuple[str, int]:
    """Live `apt update` against deb.debian.org (port of e2e_live_update.ps1).

    Network-dependent and slow under TCG: the timeout is SOFT (per resolved open
    question Q-A), so a still-progressing run reports partial evidence instead of
    pretending the check failed on its merits. Only an explicit
    `LXSELFTEST live_update PASS` exits 0 unless --allow-partial is given."""
    a = h.a
    h.boot()
    assert h.guest is not None

    def on_tick(text: str) -> None:
        last = re.findall(r"apt: reading package lists\.\.\. [^\r\n]*", text)
        if last:
            log(f"... {last[-1]}")

    found = wait_for(h.guest, [r"LXSELFTEST live_update (?:PASS|FAIL)"], h.timeout,
                     on_tick=on_tick, label="live_update verdict")
    text = h.guest.serial()
    progress = re.findall(r"apt: reading package lists\.\.\. \S+ / (\d+) pkgs", text)
    seq = [int(p) for p in progress]
    mono = all(seq[i] >= seq[i - 1] for i in range(1, len(seq)))
    terminal = re.search(r"apt: index ready - (\d+) packages", text)
    count = re.search(r"LIVE_APT_UPDATE: count=(\d+)", text)
    footprint = re.search(r"Resident_Index_Footprint = (\d+) bytes", text)
    live_pass = bool(re.search(r"LXSELFTEST live_update PASS", text))
    live_fail = regex_first(r"LXSELFTEST live_update FAIL[^\r\n]*", text)
    refusals = re.findall(r"Package_Fetcher\(tls\): stage=verify cause=[^\r\n]*", text)
    h.metrics.update({
        "progress lines": len(progress),
        "parsed pkgs monotonic": mono if progress else "vacuous (none)",
        "index ready": f"{terminal.group(1)} packages" if terminal else "not reached",
        "LIVE_APT_UPDATE count": count.group(1) if count else "not reached",
        "index footprint": f"{footprint.group(1)} bytes" if footprint else "n/a",
        "TLS verifier refusals": len(refusals),
    })
    if refusals:
        log(f"TLS fail-closed path active: x{len(refusals)} refusal(s); first: {refusals[0]}")
    if live_pass:
        ok = h.check("LXSELFTEST live_update PASS", True,
                     regex_first(r"LXSELFTEST live_update PASS[^\r\n]*", text) or "")
        if count:
            ok &= h.check("LIVE_APT_UPDATE count >= 50000", int(count.group(1)) >= 50000,
                          count.group(1))
        return ("PASS" if ok else "FAIL"), (EXIT_PASS if ok else EXIT_FAIL)
    if live_fail:
        h.check("LXSELFTEST live_update PASS", False, live_fail)
        return "FAIL", EXIT_FAIL
    if found is None and (progress or terminal):
        h.check("live update still progressing at the soft timeout (Q-A: acceptable "
                "partial evidence)", True,
                f"{len(progress)} progress lines, "
                f"last={seq[-1] if seq else 'n/a'} pkgs, terminal="
                f"{'yes' if terminal else 'no'}")
        return ("PARTIAL", EXIT_PASS) if a.allow_partial else ("PARTIAL", EXIT_FAIL)
    h.check("LXSELFTEST live_update PASS", False, "no verdict marker and no progress")
    return "INCONCLUSIVE", EXIT_FAIL


def mode_bigindex(h: Harness) -> tuple[str, int]:
    """Big-index apt parse repro/regression (port of e2e_bigindex.ps1).

    Default (regression gate): requires `LXSELFTEST bigindex PASS` and NO
    `[EXC #14]`. `--expect-crash` inverts it into a repro check (exit 0 iff the
    #14 page fault is observed) for issue #17's before/after evidence."""
    a = h.a
    if a.port != 8000:
        warn(f"--port {a.port}: the in-guest harness hard-codes http://10.0.2.2:8000")
    h.mini_repo_snapshot.take()
    h.boot()
    assert h.guest is not None
    h.mirror_start(["bigindex", str(a.stanzas), str(a.port)], a.port)
    found = wait_for(h.guest, [r"EXC #14", r"LXSELFTEST bigindex (?:PASS|FAIL)"],
                     h.timeout, label="bigindex verdict")
    time.sleep(a.settle)
    text = h.guest.serial()
    crash = regex_first(r"EXC #14[^\r\n]*", text)
    passed = regex_first(r"LXSELFTEST bigindex PASS[^\r\n]*", text)
    failed = regex_first(r"LXSELFTEST bigindex FAIL[^\r\n]*", text)
    pkgs = re.findall(r"(\d+) packages", text)
    h.metrics.update({
        "stanzas served": a.stanzas,
        "crash marker": crash or "none",
        "terminal marker": passed or failed or "none",
        "packages parsed": pkgs[-1] if pkgs else "n/a",
        "in-ram variant": bool(a.in_ram),
    })
    if a.expect_crash:
        ok = h.check("[EXC #14] crash reproduced (repro mode)", bool(crash),
                     crash or "no crash marker")
        if not crash and passed:
            h.check("big index parsed cleanly (the bug is not present in this build)", True,
                    passed)
        return ("CRASH-REPRODUCED" if ok else "NO-CRASH"), (EXIT_PASS if ok else EXIT_FAIL)
    ok = h.check("no [EXC #14] page fault", not crash, crash or "absent")
    ok &= h.check("LXSELFTEST bigindex PASS", bool(passed), passed or (failed or "no marker"))
    ok &= h.check("no bigindex FAIL marker", not failed, failed or "absent")
    # Scale check (issue #17 follow-up): a bare PASS marker is not evidence that a
    # LARGE index parsed. `--stanzas 1` yields "PASS (streaming; 1 packages)" and
    # exit 0, which is indistinguishable from a real 60k regression pass — the gate
    # would silently stop detecting the very crash class it exists for. Compare the
    # parsed count against what the run actually asked for.
    if not a.expect_crash:
        expected = 12_000 if a.in_ram else a.stanzas
        label = "in-RAM variant (fixed 12000 stanzas)" if a.in_ram else f"--stanzas {a.stanzas}"
        counts = [int(c) for c in
                  re.findall(r"(\d+) packages", passed or "") + re.findall(r"(\d+) packages", text)]
        parsed = max(counts) if counts else None
        ok &= h.check(
            f"parsed scale >= {expected} packages ({label})",
            parsed is not None and parsed >= expected,
            f"parsed={parsed if parsed is not None else 'n/a'} (expected >= {expected})"
            + ("" if parsed is None else
               " - a below-scale index cannot prove the #14 parse path at size"),
        )
    return ("PASS" if ok else "FAIL"), (EXIT_PASS if ok else EXIT_FAIL)


def parse_script(path: pathlib.Path) -> list[dict]:
    steps: list[dict] = []
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        head, _, rest = line.partition(" ")
        head, rest = head.lower(), rest.strip()
        if head == "type":
            steps.append({"op": "type", "text": rest})
        elif head == "cmd":
            steps.append({"op": "cmd", "text": rest})
        elif head == "key":
            steps.append({"op": "key", "name": rest})
        elif head == "sleep":
            steps.append({"op": "sleep", "sec": float(rest)})
        elif head == "wait":
            pat, _, tmo = rest.rpartition(" ")
            try:
                seconds = float(tmo)
                steps.append({"op": "wait", "regex": pat.strip(), "timeout": seconds})
            except ValueError:
                steps.append({"op": "wait", "regex": rest, "timeout": None})
        elif head == "expect":
            steps.append({"op": "expect", "regex": rest})
        elif head == "note":
            steps.append({"op": "note", "text": rest})
        else:
            raise SetupError(f"{path}:{lineno}: unknown script op {head!r}")
    return steps


def mode_shell(h: Harness) -> tuple[str, int]:
    """Generic interactive mode: type shell commands into the guest and assert on
    the serial output. `--script FILE` supports a small DSL:

        # comment
        cmd lxrun /mnt/usr/bin/hello-pagh     # type + Enter
        type selftest                          # type without Enter
        key ret                                # raw QEMU key name
        wait LXSELFTEST end_to_end_run PASS 30 # block until this regex (timeout s)
        sleep 2
        expect hello from apt                  # asserted at the end
    """
    a = h.a
    h.boot(need_keyboard=True)
    assert h.kb is not None and h.guest is not None
    if not wait_for_shell(h.guest, h.kb, a, a.boot_timeout):
        raise HarnessError(f"the shell prompt never appeared within {a.boot_timeout:.0f}s; "
                           f"serial tail:\n{tail(h.guest.serial())}")
    steps: list[dict] = list(parse_script(pathlib.Path(a.script))) if a.script else []
    steps += [{"op": "cmd", "text": c} for c in (a.cmd or [])]
    expects = [s["regex"] for s in steps if s["op"] == "expect"] + list(a.expect or [])
    declared = 0
    for step in steps:
        if step["op"] == "cmd":
            log(f"type: {step['text']}")
            h.kb.text(step["text"])
            h.kb.key("ret")
        elif step["op"] == "type":
            log(f"type: {step['text']}")
            h.kb.text(step["text"])
        elif step["op"] == "key":
            h.kb.key(step["name"])
        elif step["op"] == "sleep":
            time.sleep(step["sec"])
        elif step["op"] == "note":
            log(f"note: {step['text']}")
        elif step["op"] == "wait":
            declared += 1
            tmo = step["timeout"] if step["timeout"] is not None else h.timeout
            log(f"waiting up to {tmo:.0f}s for /{step['regex']}/")
            if not wait_for(h.guest, [step["regex"]], tmo, label=f"/{step['regex']}/"):
                h.check(f"wait /{step['regex']}/", False, "timeout")
                return "FAIL", EXIT_FAIL
    time.sleep(a.settle)
    text = h.guest.serial()
    ok = True
    if not steps:
        # No interaction requested: the run is a boot smoke check.
        ok &= h.check("shell reached", SHELL_MARK in text or SHELL_PROMPT in text)
    if declared == 0:
        h.check("interaction steps executed", True, f"{len(steps)} step(s)")
    for pattern in expects:
        found = re.search(pattern, text)
        ok &= h.check(f"expected /{pattern}/", bool(found),
                      found.group(0).strip() if found else "not found")
    for extra in h.extra_evidence:
        found = re.search(extra, text)
        h.check(f"extra marker /{extra}/ (best effort)", bool(found),
                found.group(0).strip() if found else "not observed")
    return ("PASS" if ok else "FAIL"), (EXIT_PASS if ok else EXIT_FAIL)


def resolve_log(path: str, default: str) -> pathlib.Path:
    p = pathlib.Path(path) if path else ROOT / default
    return p if p.is_absolute() else ROOT / p


def mode_smoke(h: Harness) -> tuple[str, int]:
    """Log-based smoke criteria R4.1/R4.2/R7.4 (port of smoke_assertions.ps1).

    Runs no QEMU: it reads captured serial logs and (unless skipped) performs the
    debug build+link check with the project's own build driver."""
    a = h.a
    local = resolve_log(a.local_log, "serial_e2e.log")
    live = resolve_log(a.live_log, "serial_live.log")
    h.serial_log = local  # the report/JSON must point at the evidence that was read
    local_text, live_text = read_text(local), read_text(live)
    log(f"reading {local} ({len(local_text)} bytes)"
        + (f" and {live} ({len(live_text)} bytes)" if live_text else " (no live log)"))
    ok = True

    # R4.1 — release build links AND boots (reaches the shell).
    shell = "Welcome to pagh OS Shell!" in local_text
    prompt = SHELL_PROMPT in local_text
    ok &= h.check("R4.1 release build links AND boots (shell reached)", shell or prompt,
                  ("'Welcome to pagh OS Shell!' in " + str(local)) if shell
                  else (f"'{SHELL_PROMPT}' prompt in {local}" if prompt else "unproven"))

    # R4.2 — debug profile compiles+links; the local-mirror log exercises the same
    # index pipeline functionally.
    if a.skip_debug_build:
        h.check("R4.2 debug build link (skipped)", True, "--skip-debug-build")
    else:
        log("R4.2: running the debug build+link check (tools/build.py build) ...")
        proc = subprocess.run([sys.executable, str(TOOLS / "build.py"), "build"],
                              cwd=ROOT, capture_output=True, text=True)
        linked = proc.returncode == 0 and "linked:" in proc.stdout
        ok &= h.check("R4.2 debug build compiles + links", linked,
                      (proc.stdout.strip().splitlines() or ["no output"])[-1]
                      if proc.stdout.strip() else f"exit {proc.returncode}")
        if not linked:
            print(proc.stdout[-2000:], proc.stderr[-2000:], file=sys.stderr)
    idx = re.search(r"(?:apt: index ready - (\d+) packages"
                    r"|LXSELFTEST apt_e2e: index loaded \((\d+) packages\))", local_text)
    h.check("R4.2 functional index-pipeline evidence in the local log", bool(idx),
            idx.group(0).strip() if idx else "none (run `local-mirror` first)")

    # R7.4 — HTTPS server authentication evidenced (positive handshake or a
    # refusal that names the verifier check). Plain-HTTP index lines do not count.
    positive = [
        (r"LXSELFTEST https_get PASS", "TLS 1.3 handshake completed (chain + SAN + clock + CertificateVerify) and the body decrypted"),
        (r"Package_Fetcher\(tls\): stage=response host=.*cause=Status\(", "TLS handshake completed; only the HTTP status was non-200"),
    ]
    negative = [
        (r"Package_Fetcher\(tls\): stage=verify cause=InvalidCertificate", "handshake refused: chain/SAN/clock check failed"),
        (r"Package_Fetcher\(tls\): stage=verify cause=InvalidSignatureScheme", "handshake refused: unacceptable CertificateVerify scheme"),
        (r"Package_Fetcher\(tls\): stage=verify cause=InvalidSignature", "handshake refused: CertificateVerify did not verify"),
    ]
    hit, why, where = None, "", ""
    for src, text in ((local, local_text), (live, live_text)):
        for pattern, reason in positive + negative:
            m = re.search(pattern, text)
            if m and hit is None:
                hit, why, where = m.group(0).strip(), reason, str(src)
    ok &= h.check("R7.4 HTTPS server authentication evidenced", bool(hit),
                  f"{hit} ({why}; {where})" if hit else
                  "neither 'LXSELFTEST https_get PASS' nor a 'Package_Fetcher(tls): "
                  "stage=verify cause=...' refusal found")
    h.metrics.update({"local log": str(local), "live log": str(live)})
    if a.soft:
        log("--soft: reporting only, exit code forced to 0")
        return ("PASS" if ok else "UNPROVEN"), EXIT_PASS
    return ("PASS" if ok else "FAIL"), (EXIT_PASS if ok else EXIT_FAIL)


def mode_boot_check(h: Harness) -> tuple[str, int]:
    """Log-only boot-identity check (no QEMU): the guard's own test surface."""
    text = read_text(h.serial_log)
    info = boot_integrity(text, h.a.tag or None)
    print(json.dumps(info, indent=2))
    if info["level"] == "verified":
        return "BOOT-IDENTITY: VERIFIED", EXIT_PASS
    if info["level"] == "fail":
        print(f"[e2e] BOOT IDENTITY FAIL: {info['detail']}", file=sys.stderr, flush=True)
        return "BOOT-IDENTITY: FAIL", EXIT_FAIL
    print(f"[e2e] BOOT IDENTITY UNVERIFIABLE: {info['detail']}", file=sys.stderr, flush=True)
    return "BOOT-IDENTITY: UNVERIFIABLE", EXIT_UNVERIFIED


def mode_restore(_h: Harness) -> tuple[str, int]:
    art = Artifacts(keep=False)
    data = art.stale_manifest()
    if not data:
        warn("no backup manifest in .cache/e2e_backup — nothing to restore "
             "(the backup is removed after every successful run)")
        return "NOTHING-TO-DO", EXIT_PASS
    art.heal_stale()
    return "RESTORED", EXIT_PASS


MODES = {
    "selftest": mode_selftest,
    "local-mirror": mode_local_mirror,
    "live-update": mode_live_update,
    "bigindex": mode_bigindex,
    "shell": mode_shell,
    "smoke": mode_smoke,
    "boot-check": mode_boot_check,
    "restore": mode_restore,
}


# ───────────────────────── CLI ─────────────────────────


def sweep_stale_run_dirs() -> None:
    """Remove per-run NVRAM/stage dirs left behind by a killed run.

    Runs inside the cross-worktree lock, so no live run can own one of these.
    """
    for d in sorted(CACHE.glob("e2e_run_*")):
        if d.is_dir() and not d.name.endswith(f"_{os.getpid()}"):
            shutil.rmtree(d, ignore_errors=True)


def build_parser() -> argparse.ArgumentParser:
    common = argparse.ArgumentParser(add_help=False)
    g = common.add_argument_group("build")
    g.add_argument("--features", default="", metavar="LIST",
                   help="extra cargo features (comma-separated); ADDED to the mode's "
                        "required features")
    g.add_argument("--debug", action="store_true", help="debug profile (default release)")
    g.add_argument("--limine-dir", default=os.environ.get("LIMINE_DIR", ""),
                   help="Limine dir (default: any local limine*/ tree via tools/limine.py)")
    common.add_argument("--stage-dir", default=None,
                        help="boot tree directory (default iso_root)")
    q = common.add_argument_group("qemu")
    q.add_argument("--cpu", default=os.environ.get("PAGH_QEMU_CPU", "max"),
                   help="QEMU CPU model (default max: the TLS path needs RDSEED/RDRAND)")
    q.add_argument("--memory", default="1024M")
    q.add_argument("--disk", default=None,
                   help="data disk image (default: per-run scratch copy of disk.img)")
    q.add_argument("--reuse-scratch-disk", action="store_true",
                   help="reuse .cache/e2e_disk.img instead of re-copying disk.img")
    q.add_argument("--ovmf", default=os.environ.get("OVMF", "OVMF.fd"))
    q.add_argument("--provision", choices=["skip", "yes", "ask"], default="skip",
                   help="answer to the first-boot [Y/n] python3 question "
                        "(default skip = 'n'; the shell stays usable)")
    q.add_argument("--key-delay", type=float, default=0.15,
                   help="delay between injected keystrokes, seconds")
    q.add_argument("--poll", type=float, default=1.5, help="serial poll interval, seconds")
    t = common.add_argument_group("timeouts")
    t.add_argument("--timeout", type=float, default=None,
                   help="max seconds to wait for the primary marker (mode default)")
    t.add_argument("--boot-timeout", type=float, default=300.0,
                   help="max seconds to reach the shell prompt")
    t.add_argument("--settle", type=float, default=4.0,
                   help="extra seconds to let late output land before teardown")
    r = common.add_argument_group("evidence")
    r.add_argument("--serial-log", default=None, help="serial capture path (mode default)")
    r.add_argument("--json", default=None, help="summary JSON path (default .cache/…)")
    r.add_argument("--evidence-regex", action="append", default=[],
                   help="evidence filter (repeatable; overrides the mode default)")
    r.add_argument("--evidence-limit", type=int, default=200)
    r.add_argument("--expect", action="append", default=[],
                   help="extra regex that MUST match the serial log (repeatable)")
    r.add_argument("--extra-wait", action="append", default=[],
                   help="extra regex reported best-effort at the end (repeatable)")
    common.add_argument("--rtc", default=None,
                        help="QEMU -rtc string, e.g. 'base=2020-01-01' to boot with a clock "
                             "before 2025-01-01 (exercise the TLS clock gate / #32 ClockUnset "
                             "case end to end)")
    common.add_argument("--lock-file", default=os.environ.get("PAGH_E2E_LOCK", ""),
                        help="cross-worktree run lock (default /tmp/pagh-e2e.lock; "
                             "$PAGH_E2E_LOCK)")
    common.add_argument("--require-boot-proof", action="store_true",
                        help="fail (exit 2) unless the guest provably printed the per-run "
                            "BOOTID of the staged image; becomes the default once the kernel "
                            "half (tools/e2e_bootid_kernel.patch) is in main")
    common.add_argument("--keep-artifacts", action="store_true",
                        help="keep the tested (feature) ELF and logs instead of "
                             "restoring the default iso_root/pagh.elf")

    p = argparse.ArgumentParser(
        prog="e2e.py", parents=[common], description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="mode", required=True)

    sel = sub.add_parser("selftest", parents=[common],
                         help="boot the default kernel, run the in-guest `selftest` shell "
                              "command, collect the PASS/FAIL summary")
    sel.add_argument("--cmd", action="append", default=[],
                     help="shell command to type (repeatable; default: selftest)")

    lm = sub.add_parser("local-mirror", parents=[common],
                        help="deterministic local-mirror apt E2E (replaces "
                             "e2e_local_mirror.ps1)")
    lm.add_argument("--port", type=int, default=8000)

    lu = sub.add_parser("live-update", parents=[common],
                        help="live apt update against deb.debian.org (replaces "
                             "e2e_live_update.ps1; soft timeout)")
    lu.add_argument("--allow-partial", action="store_true",
                    help="exit 0 when the run is still progressing at the soft timeout")

    bi = sub.add_parser("bigindex", parents=[common],
                        help="big-index apt parse repro (replaces e2e_bigindex.ps1)")
    bi.add_argument("--stanzas", type=int, default=60000)
    bi.add_argument("--port", type=int, default=8000)
    bi.add_argument("--in-ram", action="store_true", help="also enable lx_bigindex_inram")
    bi.add_argument("--allow-small-index", action="store_true",
                    help="permit --stanzas below the regression scale floor (see "
                         "BIGINDEX_MIN_STANZAS); the verdict is then NOT #17 evidence")
    bi.add_argument("--expect-crash", action="store_true",
                    help="invert the verdict: PASS iff [EXC #14] is reproduced")

    sh = sub.add_parser("shell", parents=[common],
                        help="generic interactive mode: type shell commands, assert on serial")
    sh.add_argument("--cmd", action="append", default=[],
                    help="shell command to type + Enter (repeatable)")
    sh.add_argument("--script", default=None,
                    help="interaction script (see the module docstring for the DSL)")

    sm = sub.add_parser("smoke", parents=[common],
                        help="log-based smoke criteria R4.1/R4.2/R7.4 (replaces "
                             "smoke_assertions.ps1)")
    sm.add_argument("--local-log", default="serial_e2e.log")
    sm.add_argument("--live-log", default="serial_live.log")
    sm.add_argument("--skip-debug-build", action="store_true")
    sm.add_argument("--soft", action="store_true",
                    help="report only: always exit 0 (the PowerShell script's behaviour)")

    bc = sub.add_parser("boot-check", parents=[common],
                        help="verify a captured serial log against the BOOTID boot-identity "
                             "check (0 verified / 1 fail / 3 unverifiable); used by the "
                             "synthetic guard tests")
    bc.add_argument("--tag", default=None, help="expected BOOTID tag (per-run)")

    sub.add_parser("restore", parents=[common],
                   help="restore iso_root/pagh.elf from the last-run backup "
                        "(crash recovery)")
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    mode = args.mode
    # Sub-scale refusal is decided BEFORE any build/stage work: it is a property of
    # the request, not of the tree, so a broken or mid-edit working tree must not
    # mask it as a build error (issue #17 follow-up; see BIGINDEX_MIN_STANZAS).
    if (mode == "bigindex" and args.stanzas < BIGINDEX_MIN_STANZAS
            and not args.allow_small_index):
        print(f"[e2e] bigindex: --stanzas {args.stanzas} is below the regression scale "
              f"floor ({BIGINDEX_MIN_STANZAS}). The #14 parse-stage crash was observed "
              f"at ~5459 stanzas, so a smaller index cannot exercise that path and a "
              f"green verdict from it is a false negative, not #17 evidence. Refusing "
              f"to run (exit 2); pass --allow-small-index only for a deliberate "
              f"sub-scale control run.", file=sys.stderr, flush=True)
        print("\nE2E RESULT: REFUSED (sub-scale index) (exit 2)", flush=True)
        return EXIT_SETUP
    h = Harness(args, mode)
    verdict, code = "FAIL", EXIT_FAIL
    try:
        with RunLock(pathlib.Path(args.lock_file) if args.lock_file else None):
            sweep_stale_run_dirs()
            if mode not in ("smoke", "restore", "boot-check"):
                h.artifacts.heal_stale()
                h.build_and_stage()
            verdict, code = MODES[mode](h)
            if mode not in ("smoke", "restore", "boot-check"):
                h.verify_boot()  # may raise BootIdentityError -> REFUSED
    except KeyboardInterrupt:
        verdict, code = "INTERRUPTED", 130
    except SetupError as exc:
        print(f"[e2e] setup error: {exc}", file=sys.stderr, flush=True)
        verdict, code = "SETUP-ERROR", EXIT_SETUP
    except BootIdentityError as exc:
        print(f"[e2e] REFUSED: cannot prove which image the guest executed: {exc}",
              file=sys.stderr, flush=True)
        verdict, code = "REFUSED (boot identity)", EXIT_SETUP
    except (HarnessError, subprocess.CalledProcessError) as exc:
        print(f"[e2e] harness failure: {exc}", file=sys.stderr, flush=True)
        verdict, code = "ERROR", EXIT_FAIL
    finally:
        h.cleanup()
    if mode not in ("restore",):
        h.print_report(verdict)
        h.write_summary(verdict, code)
    print(f"\nE2E RESULT: {verdict} (exit {code})", flush=True)
    return code


def _sigterm(signum, _frame):  # pragma: no cover - signal plumbing
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, _sigterm)
    sys.exit(main())
