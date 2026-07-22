#!/usr/bin/env python3
"""Cross-platform build/link/stage/run driver for pagh."""
from __future__ import annotations
import argparse, os, pathlib, shutil, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-none"


def run(cmd: list[str], *, cwd: pathlib.Path = ROOT) -> None:
    print("+", " ".join(map(str, cmd)), flush=True)
    subprocess.run(cmd, cwd=cwd, check=True)


def require(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise SystemExit(f"error: required tool not found: {name}")
    return path


def rust_lld() -> str:
    rustc = require("rustc")
    sysroot = subprocess.check_output([rustc, "--print", "sysroot"], text=True).strip()
    exe = "rust-lld.exe" if os.name == "nt" else "rust-lld"
    candidates = list(pathlib.Path(sysroot).glob(f"lib/rustlib/*/bin/{exe}")) + list(pathlib.Path(sysroot).rglob(exe))
    if not candidates:
        raise SystemExit("error: rust-lld not found in the pinned Rust toolchain")
    return str(candidates[0])


def build(profile: str, features: str) -> pathlib.Path:
    cmd = [require("cargo"), "build", "--locked"]
    if profile == "release": cmd.append("--release")
    if features: cmd += ["--features", features]
    run(cmd)
    out = ROOT / "target" / TARGET / profile
    archive = out / ("pagh.lib" if os.name == "nt" else "libpagh.a")
    if not archive.exists():
        # Rust staticlibs on Windows GNU/MSVC installations may still use lib*.a.
        archive = out / "libpagh.a"
    if not archive.exists(): raise SystemExit(f"error: kernel archive not found: {archive}")
    elf = out / "pagh.elf"
    run([rust_lld(), "-flavor", "gnu", "-T", str(ROOT / "linker.ld"), "-nostdlib", "-static",
         "--whole-archive", str(archive), "--no-whole-archive", "-o", str(elf)])
    return elf


def stage(elf: pathlib.Path, limine: pathlib.Path) -> pathlib.Path:
    loader = limine / "BOOTX64.EFI"
    if not loader.exists(): raise SystemExit(f"error: Limine loader missing: {loader}")
    dest = ROOT / "iso_root"
    shutil.rmtree(dest, ignore_errors=True)
    (dest / "EFI" / "BOOT").mkdir(parents=True)
    shutil.copy2(elf, dest / "pagh.elf")
    shutil.copy2(loader, dest / "EFI" / "BOOT" / "BOOTX64.EFI")
    conf = (ROOT / "boot" / "limine.conf").read_text(encoding="utf-8")
    (dest / "limine.conf").write_text(conf, encoding="utf-8")
    (dest / "EFI" / "BOOT" / "limine.conf").write_text(conf, encoding="utf-8")
    return dest


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("command", choices=["build", "stage", "run"], nargs="?", default="build")
    p.add_argument("--release", action="store_true")
    p.add_argument("--features", default="")
    p.add_argument("--limine-dir", default=os.environ.get("LIMINE_DIR", "limine-12.3.1"))
    p.add_argument("--ovmf", default=os.environ.get("OVMF", "OVMF.fd"))
    p.add_argument("--disk", default=os.environ.get("PAGH_DISK", "disk.img"))
    a = p.parse_args()
    elf = build("release" if a.release else "debug", a.features)
    print(f"linked: {elf}")
    if a.command == "build": return
    esp = stage(elf, ROOT / a.limine_dir)
    if a.command == "stage": return
    ovmf, disk = ROOT / a.ovmf, ROOT / a.disk
    if not ovmf.exists(): raise SystemExit(f"error: OVMF missing: {ovmf}")
    qemu, qimg = require("qemu-system-x86_64"), require("qemu-img")
    if not disk.exists(): run([qimg, "create", "-f", "raw", str(disk), "64M"])
    run([qemu, "-bios", str(ovmf), "-drive", f"file=fat:rw:{esp},format=raw",
         "-drive", f"file={disk},format=raw,if=none,id=hd0", "-device", "virtio-blk-pci,drive=hd0",
         "-netdev", "user,id=net0,hostfwd=tcp::5555-:7,hostfwd=udp::5555-:7",
         "-device", "virtio-net-pci,netdev=net0", "-m", "512M", "-serial", "stdio",
         "-no-reboot", "-no-shutdown", "-d", "int,cpu_reset,guest_errors", "-D", "qemu_debug.log"])

if __name__ == "__main__":
    main()
