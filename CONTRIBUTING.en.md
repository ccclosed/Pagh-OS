# Contributing to pagh

Thanks for your interest in `pagh` — a small 64-bit OS kernel written in Rust
(`#![no_std]`, booted via Limine, run under QEMU/OVMF). This is a hobby/educational
project, so clean code, clear commits, and preserving the build invariants matter here.
This document explains what to do and how, so your contribution gets merged smoothly.

> A Russian version is available in [`CONTRIBUTING.md`](CONTRIBUTING.md).

---

## 1. Getting started

1. Read [`README.md`](README.md) in full — it covers the architecture, the source layout
   (`src/`), and the build invariants. Changing the kernel without that context is risky.
2. Find or open an issue for what you want to do. For larger changes, discuss the approach
   in the issue first so you don't do work that won't be merged.
3. Fork the repo and branch off `main` (see the branches & commits section).

### Required environment

- **Rust nightly** with the `rust-src` component (needed for `build-std`):
  ```sh
  rustup toolchain install nightly
  rustup component add rust-src --toolchain nightly
  ```
  `rust-lld` ships with the toolchain and is used as the linker.
- **QEMU** (`qemu-system-x86_64`) and `qemu-img` on your `PATH`.
- Two local blobs in the project root (they are git-ignored — do NOT commit them):
  - `OVMF.fd` — UEFI firmware for QEMU.
  - `limine-12.3.1/` — the Limine bootloader tree (must contain `BOOTX64.EFI`).

`disk.img` is created automatically on first run — don't commit it either.

---

## 2. Build and run

The whole build/link/run pipeline goes through `run.cmd` (Windows):

```bat
run.cmd build           :: cargo build + link PAGH.elf
run.cmd run             :: build + link + boot in QEMU (default)
run.cmd run release     :: release build
```

Static library only:

```sh
cargo build            # debug
cargo build --release  # release
```

Exit QEMU: `Ctrl-A`, then `X`.

---

## 3. Tests are required

Any logic change must be covered and/or verified by tests. The project has two testing
layers:

1. **In-QEMU self-test** (`src/test.rs`) — 27 correctness properties (P1–P27). Run it from
   the shell with `selftest`; results print over serial as `ok`/`FAIL` lines.
2. **Host property tests** (`host-tests/`) — `proptest` for logic that extracts cleanly to
   the host. It is a separate, workspace-excluded crate that builds for the host triple:
   ```sh
   cd host-tests && cargo test
   ```

Rules:
- If you change pure logic (paths, line editor, history, decoder, journal, ext2, etc.),
  add/update the corresponding property.
- If you touch a hardware-dependent path, run `selftest` in QEMU and confirm every line is
  `ok`.
- A PR must not break the existing P1–P27 properties.

> Don't pull in test frameworks or dependencies for a single test — use the mechanisms the
> project already adopts (`src/test.rs` and `host-tests/`).

---

## 4. Build invariants (do NOT break)

These constraints are enforced across the codebase and must be preserved:

- `#![no_std]`, `panic = "abort"` (both dev and release).
- Custom target `x86_64-unknown-none.json` with
  `build-std = [core, compiler_builtins, alloc]`.
- Limine request statics live in the `.requests` section.
- Higher-half load address `0xffffffff80000000` (`linker.ld`).
- Frame pointers forced on (`-Cforce-frame-pointers=yes`) — required for the panic stack
  trace.
- `host-tests/` must NOT become a member of the kernel workspace (it builds for the host
  target).

---

## 5. Code style

- **Zero-warning build.** The tree builds with zero warnings — keep that bar. Before a PR,
  run:
  ```sh
  cargo fmt --all
  cargo clippy
  ```
- **Minimize and document `unsafe`.** Every `unsafe` block carries a `// SAFETY:` comment
  explaining why it is sound.
- **Privileged instructions only through `arch::cpu`.** Outside `arch` there should be no
  inline `asm!`, except the unavoidable stubs in `task::switch` and the GDT segment reload.
- **No references to `static mut`.** Reach global mutable state through
  `SyncUnsafeCell`/atomics to avoid `static_mut_refs`.
- **Separate pure logic from I/O.** Keep logic (path normalization, the editor model,
  parsing, etc.) pure and property-testable; keep the I/O layer thin.
- Follow the existing `src/` layout (see README). Put new modules in the right directory
  and wire them in idiomatically.

---

## 6. Branches, commits, and PRs

- Branch off `main` with a meaningful name: `feat/<short>`, `fix/<short>`,
  `docs/<short>`.
- Keep commits small and atomic, with imperative messages that say what they do
  ("add ext2 dir iteration", not "fixes"). Conventional Commits is preferred
  (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`).
- Don't commit generated/local artifacts: `target/`, `host-tests/target/`, `iso_root/`,
  `PAGH.elf`, `disk.img`, `OVMF.fd`, `limine-12.3.1/`, QEMU logs, IDE folders. These are
  already in `.gitignore` — don't add them through workarounds.

### Pre-PR checklist

- [ ] `run.cmd build` (debug) passes with no errors and no warnings.
- [ ] `cargo fmt --all` and `cargo clippy` are clean.
- [ ] `cd host-tests && cargo test` is green (if portable logic was touched).
- [ ] `selftest` in QEMU passes with no `FAIL` (if kernel code was touched).
- [ ] The build invariants from section 4 are preserved.
- [ ] New `unsafe` is annotated with `// SAFETY:`.
- [ ] Docs (`README.md`, comments) updated if behavior changed.
- [ ] No stray/generated files in the diff.

### PR description

Briefly state: what changes and why, how it was tested (host tests / `selftest` / manual
QEMU run), and a link to the related issue. For behavior changes, include before/after
(serial output, framebuffer screenshot, etc.).

---

## 7. Reporting bugs

When opening an issue, please include where possible:
- build mode (`debug`/`release`) and nightly version (`rustc +nightly --version`);
- reproduction steps and expected vs. actual behavior;
- relevant serial output and/or a snippet of `qemu_debug.log`;
- for a panic, the stack trace (the kernel prints it by walking the RBP chain).

---

Thanks for contributing to pagh. Keep changes focused, tests green, and the build
warning-free, and review will be quick.
