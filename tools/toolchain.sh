#!/usr/bin/env bash
# Toolchain guard for the shell build/run entry points (build.sh, run.sh).
#
# The kernel needs the toolchain pinned in `rust-toolchain.toml` (a nightly with
# `rust-src` for build-std and `rust-lld`). `.cargo/config.toml` also declares
# `json-target-spec = true`, an UNSTABLE cargo option, because the kernel builds
# for the custom bare-metal target `x86_64-unknown-none.json`.
#
# When a distro-packaged cargo shadows the rustup shim, none of that holds and
# the failure is thoroughly misleading: the stable cargo does not even get as far
# as the target spec, it dies with
#
#     error: `.json` target specs require -Zjson-target-spec to be added to the
#     cargo invocation
#
# which says nothing about the real problem (wrong cargo, no pinned nightly).
# This guard turns that into a sentence naming the cause and the fix.
#
# `tools/build.py` does not need it: it resolves the toolchain through rustup the
# same way, but it is the documented cross-platform entry point and the one CI
# mirrors.

# Sourced, not executed: only define the function.
pagh_require_pinned_toolchain() {
  local cargo_bin rustc_bin cargo_ver
  cargo_bin="$(command -v cargo || true)"
  rustc_bin="$(command -v rustc || true)"

  if [[ -z "$cargo_bin" || -z "$rustc_bin" ]]; then
    echo "error: cargo/rustc not found in PATH" >&2
    echo "       install the pinned toolchain: rustup toolchain install \"\$(sed -n 's/^channel = \"\\(.*\\)\"/\\1/p' "$PWD/rust-toolchain.toml")\"" >&2
    exit 1
  fi

  cargo_ver="$(cargo --version 2>/dev/null || true)"

  # The pinned toolchain is a nightly; a stable cargo (whatever the distro)
  # cannot build this crate. Only the channel matters, not the exact date: the
  # rustup shim honors rust-toolchain.toml and picks the pinned build for us.
  if [[ "$cargo_ver" != *-nightly* ]]; then
    {
      echo "error: cargo is not a nightly: $cargo_ver"
      echo "       ($cargo_bin)"
      echo
      echo "       This kernel needs the toolchain pinned in rust-toolchain.toml:"
      echo "       a nightly with rust-src (build-std) and rust-lld, because it"
      echo "       builds the custom target x86_64-unknown-none.json. A stable or"
      echo "       distro-packaged cargo fails with a message about"
      echo "       '-Zjson-target-spec' that does not name the real problem."
      echo
      echo "       Fix one of these ways:"
      echo "         1. put rustup first in PATH (recommended):"
      echo "              . \"\$HOME/.cargo/env\"      # then rerun this script"
      echo "            add that line to ~/.bashrc to make it permanent."
      echo "         2. or build through the cross-platform driver, which"
      echo "            resolves the pinned toolchain itself:"
      echo "              python tools/build.py run --release"
    } >&2
    exit 1
  fi

  # Nightly, but rust-lld lives in the *active* toolchain's sysroot. `rustc
  # --print sysroot` above already reflects that, so a missing rust-lld is
  # reported where it is looked up (build.sh/run.sh), not here.
  return 0
}
