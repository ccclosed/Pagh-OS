#!/usr/bin/env bash
# Install host dependencies on common Linux distributions.
set -Eeuo pipefail

if command -v apt-get >/dev/null; then
  sudo apt-get update
  sudo apt-get install -y qemu-system-x86 qemu-utils ovmf python3 curl binutils musl-tools
elif command -v dnf >/dev/null; then
  sudo dnf install -y qemu-system-x86 qemu-img edk2-ovmf python3 curl binutils musl-gcc
elif command -v pacman >/dev/null; then
  sudo pacman -S --needed qemu-system-x86 qemu-img edk2-ovmf python curl binutils musl
else
  echo "Unsupported package manager. Install QEMU, qemu-img, OVMF, Python 3 and rustup manually." >&2
  exit 1
fi

if ! command -v rustup >/dev/null; then
  echo "rustup is missing. Install it from https://rustup.rs and rerun this script." >&2
  exit 1
fi

rustup toolchain install nightly-2026-06-15 --profile minimal --component rust-src rustfmt clippy llvm-tools-preview
cat <<'EOF'
Host dependencies installed.

Limine is downloaded automatically on first build/run (tools/limine.py
fetches the latest binary release into limine/). To prefetch now:
  python3 tools/limine.py

Or place any Limine BOOTX64.EFI manually and set LIMINE_EFI=/path/to/BOOTX64.EFI.

Then run:
  ./run.sh --release
EOF
