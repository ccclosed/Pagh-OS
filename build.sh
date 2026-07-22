#!/usr/bin/env bash
# Build and link the pagh kernel on Linux.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

PROFILE=debug
FEATURES=""
STAGE=0

usage() {
  cat <<'EOF'
Usage: ./build.sh [--release] [--features LIST] [--stage]

  --release        Build the release profile
  --features LIST  Cargo feature list, e.g. lx_selftest
  --stage          Prepare iso_root/ after linking
  -h, --help       Show this help
EOF
}

while (($#)); do
  case "$1" in
    --release) PROFILE=release; shift ;;
    --features) FEATURES="${2:?--features requires a value}"; shift 2 ;;
    --stage) STAGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for tool in cargo rustc; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done

cargo_args=(build --locked)
[[ "$PROFILE" == release ]] && cargo_args+=(--release)
[[ -n "$FEATURES" ]] && cargo_args+=(--features "$FEATURES")

echo "==> Building pagh ($PROFILE)"
cargo "${cargo_args[@]}"

TARGET=x86_64-unknown-none
OUT="$ROOT/target/$TARGET/$PROFILE"
ARCHIVE="$OUT/libpagh.a"
ELF="$OUT/pagh.elf"
[[ -f "$ARCHIVE" ]] || { echo "error: kernel archive not found: $ARCHIVE" >&2; exit 1; }

SYSROOT="$(rustc --print sysroot)"
RUST_LLD="$(find "$SYSROOT/lib/rustlib" -path '*/bin/rust-lld' -type f -print -quit 2>/dev/null)"
[[ -n "$RUST_LLD" ]] || { echo "error: rust-lld not found in $SYSROOT" >&2; exit 1; }

echo "==> Linking $ELF"
"$RUST_LLD" -flavor gnu -T "$ROOT/linker.ld" -nostdlib -static \
  --whole-archive "$ARCHIVE" --no-whole-archive -o "$ELF"

echo "==> Kernel ready: $ELF"

if ((STAGE)); then
  LIMINE_DIR="${LIMINE_DIR:-$ROOT/limine-12.3.1}"
  LOADER="${LIMINE_EFI:-$LIMINE_DIR/BOOTX64.EFI}"
  if [[ ! -f "$LOADER" ]]; then
    for candidate in "$ROOT/../limine-12.3.1/BOOTX64.EFI" /usr/share/limine/BOOTX64.EFI; do
      [[ ! -f "$candidate" ]] || { LOADER="$candidate"; break; }
    done
  fi
  [[ -f "$LOADER" ]] || {
    echo "error: BOOTX64.EFI not found" >&2
    echo "set LIMINE_DIR or LIMINE_EFI to its location" >&2
    exit 1
  }

  echo "==> Staging Limine ESP in iso_root/"
  rm -rf "$ROOT/iso_root"
  mkdir -p "$ROOT/iso_root/EFI/BOOT"
  install -m 0644 "$ELF" "$ROOT/iso_root/pagh.elf"
  install -m 0644 "$LOADER" "$ROOT/iso_root/EFI/BOOT/BOOTX64.EFI"
  install -m 0644 "$ROOT/boot/limine.conf" "$ROOT/iso_root/limine.conf"
  install -m 0644 "$ROOT/boot/limine.conf" "$ROOT/iso_root/EFI/BOOT/limine.conf"
  echo "==> Staging complete"
fi
