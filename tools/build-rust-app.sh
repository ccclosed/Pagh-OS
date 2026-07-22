#!/usr/bin/env bash
# Build a statically linked Rust application for pagh's Linux compatibility ABI.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="${1:-$ROOT/rust-apps/hello}"
TARGET=x86_64-unknown-linux-musl

[[ -f "$CRATE/Cargo.toml" ]] || { echo "error: Cargo.toml not found in $CRATE" >&2; exit 1; }
command -v rustup >/dev/null || { echo "error: rustup not found" >&2; exit 1; }
command -v cargo >/dev/null || { echo "error: cargo not found" >&2; exit 1; }

rustup target add "$TARGET"
(cd "$CRATE" && [[ -f Cargo.lock ]] || cargo generate-lockfile)
cargo build --manifest-path "$CRATE/Cargo.toml" --release --locked --target "$TARGET"

NAME="$(python3 - "$CRATE/Cargo.toml" <<'PY'
import pathlib,re,sys
s=pathlib.Path(sys.argv[1]).read_text()
m=re.search(r'^name\s*=\s*"([^"]+)"',s,re.M)
if not m: raise SystemExit('package name missing')
print(m.group(1))
PY
)"
BIN="$CRATE/target/$TARGET/release/$NAME"
OUT="$ROOT/rust-apps/out"
mkdir -p "$OUT"
install -m 0755 "$BIN" "$OUT/$NAME"

if command -v readelf >/dev/null; then
  if readelf -l "$OUT/$NAME" | grep -q INTERP; then
    echo "error: generated ELF is dynamically linked and cannot run on pagh" >&2
    exit 1
  fi
fi

echo "Rust app ready: $OUT/$NAME"
echo "Copy it into the ext2 image, then run: rust /mnt/$NAME"
