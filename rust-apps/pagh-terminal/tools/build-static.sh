#!/usr/bin/env bash
# Build pagh-terminal as a statically linked x86_64-musl binary for pagh.
#
# Why not just `cargo build --target x86_64-unknown-linux-musl` inside the
# repository?  The kernel workspace's .cargo/config.toml enables `build-std`
# and a bare-metal target for the kernel.  Cargo inherits that config from the
# parent directories even when building this standalone crate, which mixes a
# build-std core with the toolchain's precompiled musl std.  Running cargo from
# a neutral directory with an explicit manifest path gets the plain rustup
# toolchain and the stock precompiled std instead.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=x86_64-unknown-linux-musl
TOOLCHAIN="${RUSTUP_TOOLCHAIN:-nightly-2026-07-12}"
OUT_DIR="$ROOT/target/static-out"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

cd "$WORKDIR"
RUSTUP_TOOLCHAIN="$TOOLCHAIN" cargo build \
  --manifest-path "$ROOT/Cargo.toml" \
  --release --locked --offline \
  --target "$TARGET" \
  --target-dir "$OUT_DIR"

BIN="$OUT_DIR/$TARGET/release/pagh-terminal"
[[ -f "$BIN" ]] || { echo "error: $BIN was not produced" >&2; exit 1; }
if command -v readelf >/dev/null; then
  if readelf -l "$BIN" | grep -q INTERP; then
    echo "error: generated ELF is dynamically linked" >&2
    exit 1
  fi
fi
install -D -m 0755 "$BIN" "$ROOT/../out/pagh-terminal"
echo "static binary: $ROOT/../out/pagh-terminal"
file "$ROOT/../out/pagh-terminal" 2>/dev/null || true
