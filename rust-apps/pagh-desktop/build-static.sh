#!/usr/bin/env bash
# Собрать статические musl-бинарники pagh-desktop и pagh-desktop-demo.
#
# Родительский Cargo-конфиг репозитория (bare-metal ядро + build-std core/alloc)
# не подходит std-крейту, поэтому сборка запускается из /tmp: cargo ищет
# .cargo/config.toml от текущего каталога, а не от --manifest-path. Явный
# RUSTUP_TOOLCHAIN нужен, потому что из /tmp rustup не видит rust-toolchain.toml.
set -Eeuo pipefail

CRATE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TOOLCHAIN="${PAGH_RUST_TOOLCHAIN:-nightly-2026-07-12}"
TARGET=x86_64-unknown-linux-musl

command -v cargo >/dev/null || { echo "error: cargo не найден" >&2; exit 1; }
rustup target add --toolchain "$TOOLCHAIN" "$TARGET" >/dev/null

OUT="$CRATE/target/$TARGET/release"
(cd /tmp && RUSTUP_TOOLCHAIN="$TOOLCHAIN" cargo build \
    --manifest-path "$CRATE/Cargo.toml" \
    --release --locked --target "$TARGET" "$@")

echo "Готово (static-pie, musl):"
file "$OUT/pagh-desktop" "$OUT/pagh-desktop-demo"
echo "Бинарники: $OUT/pagh-desktop, $OUT/pagh-desktop-demo"
