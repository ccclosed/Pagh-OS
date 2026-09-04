# AGENTS.md — guide for AI agents (and humans) working on pagh

A small 64-bit OS kernel in Rust (`#![no_std]`), booted via Limine on UEFI,
run under QEMU/OVMF. Written by AI under human supervision — read this file
before touching the code, it lists the commands that must pass and the
invariants that are easy to break silently.

## Authoritative documentation

Read in this order; do not rely on memory over these files:

| File | Contents |
|---|---|
| `README.md` | Architecture, features, build/run, honest limitations |
| `src/README.md` | Crate-root map, exact boot sequence (phase order matters) |
| `src/<subsystem>/README.md` | Per-subsystem design docs — **each folder documents itself**; update the README when you change the design |
| `CONTRIBUTING.md` | Human-facing contributor guide (RU) |
| `tools/README.md` | Build/test/E2E tooling reference |

## Commands (all four must be green before you push)

```sh
cargo build                              # debug kernel (libpagh.a)
python tools/build.py build --release    # release kernel + link (CI parity)
cargo fmt --all -- --check               # formatting gate
python tools/check_safety.py             # unsafe-policy gate
python tools/host_tests.py               # host property tests (or: cd host-tests && cargo test)
```

CI (`.github/workflows/ci.yml`) runs exactly: fmt check, debug build, release
build, host-tests, static-policy. CI is the arbiter — local green is
necessary, not sufficient.

The kernel needs the pinned nightly (`rust-toolchain.toml`) with `rust-src`
(build-std) and `rust-lld`; it links via `linker.ld` into `pagh.elf`. On
Windows use `run.cmd build|run`; on Linux `./build.sh` / `./run.sh`.
`OVMF.fd`, `limine-12.3.1/` and `disk.img` are local, git-ignored — never
commit them.

## Repo layout

- `src/` — the kernel (crate `pagh`, `staticlib`, target `x86_64-unknown-none`).
  Subsystems: `arch` (CPU + Linux compat layer in `arch/x86_64/linux/`),
  `drivers`, `fs` (ext2 + WAL), `memory` (PMM/VMM/heap), `net` (own TCP/IP
  stack, no smoltcp), `pkg` (apt), `shell`, `task`, `vfs`, `sync`, `security`,
  `debug`, plus root modules (`boot.rs`, `log.rs`, `provision.rs`, `test.rs`,
  `selftest_lx.rs`).
- `host-tests/` — proptest crate, **excluded from the kernel workspace**
  (bare-metal vs host targets). Includes pure kernel sources directly via
  `#[path]` — tests execute the same files the kernel compiles.
- `vendor/` — all dependencies vendored; `third_party/x86_64` is wired through
  `[patch.crates-io]`. Compression/TLS crates are pinned exactly
  (`miniz_oxide`, `ruzstd`, `xz4rust`, `embedded-tls`) with
  `default-features = false` — do not "upgrade" or re-enable features; the
  no_std build breaks. `vendor/**` is `-text` in `.gitattributes` (checksums
  are byte-sensitive); never re-save vendored files with converted line
  endings.

## Hard invariants (breaking these looks like "hardware" bugs)

1. **asm ABI contracts are load-bearing.** `SavedRegs`
   (`arch/x86_64/linux/regs.rs`) mirrors the push order of `int80_stub` /
   `syscall_entry` (`syscall.rs`): 15 GPRs, `rax` at offset 112, and the
   per-task **user-RSP slot at `+120`** (read/written by `execve`, `clone`,
   signal delivery). `task/switch.rs::irq32_stub` has its own frame layout
   asserted byte-for-byte in `src/test.rs`. Never reorder, and never add a
   Rust-side offset without updating the asm.
2. **Compat ⇒ `syscall` entry.** Linux Compat_Processes enter syscalls only
   through `syscall_entry`; on that path saved `rcx` = user RIP, `r11` =
   user RFLAGS, and the `+120` slot = user RSP. Code assuming this (execve,
   clone, signals) is correct *because* of this; an `int 0x80`-entering
   compat process would break it — don't add one.
3. **Single CPU, spinlocks mask IF.** The kernel spinlock disables
   interrupts while held, so an IRQ handler taking the same lock cannot
   deadlock — but a non-IRQ context must never take a lock twice
   (non-reentrant). The compat registry is additionally guarded by a
   reentrancy depth counter (`compat_lock_held`); the page-fault path
   checks it.
4. **Never hold `COMPAT_STATES` (compat registry) across blocking work** —
   extract what you need under the lock, release, do the blocking I/O,
   re-acquire to commit (see the pattern in `io_sys.rs`).
5. **`ticks()` advances only with interrupts enabled.** Blocking syscall
   handlers re-enable IF first (`linux_dispatch` does); a `hlt`-based sleep
   with IF masked sleeps forever.
6. **`create_user_process` / spawn paths run `without_interrupts`** — a
   timer tick observing a half-built CR3 corrupts scheduling.
7. **Boot order in `boot.rs` is fragile**: `enable_sse()` is first (x86-interrupt
   prologues emit `movaps`; Limine hands over with OSFXSR=0), ext2 mounts
   before interrupts are enabled, virtio enumerate needs the heap.
8. **`panic = "abort"`**: a panic anywhere kills the machine. In-QEMU
   selftests (`src/test.rs`) use `assert_kernel!` (print + continue) and must
   restore all state they touch (PMM, heap, IF, VFS). `selftest_lx` checks
   print `LXSELFTEST <name> PASS/FAIL` and return.
9. **Feature gates**: `default = ["network_packages"]` enables apt; the
   `lx_selftest` / `lx_livetest` / `lx_bigindex` harnesses must stay
   compiled-out (and boot-unchanged) when unset. TLS is deliberately
   NoVerify (VARIANT A) — encrypted, unauthenticated; see `SECURITY.md`.
10. **Unsafe policy**: every `unsafe {` in `src/security/`,
    `arch/x86_64/linux/mod.rs`, `memory/vmm.rs`, `net/tls.rs`, `pkg/apt.rs`
    needs a `SAFETY:` comment within the previous 6 lines
    (`tools/check_safety.py` enforces; the rest of the kernel follows the
    same style voluntarily — keep it).

## Testing philosophy

- **Pure logic → host property tests.** A module must stay `core`(+`alloc`)
-only to be `#[path]`-included in `host-tests/src/lib.rs` and covered by a
  `properties/pNN.rs` property file. If your change touches such a module
  (`abi`, `errno`, `validate`, `io`, `wire`, `deb`, `tar`, `apt_index`,
  `signal_frame`, …), extend/adjust the property; the `#[cfg(test)]`
  `supported_set_is_exact` list in `abi.rs` must match `is_supported`.
- **Kernel-internal state → in-QEMU selftests** (`src/test.rs`, run via the
  `selftest` shell command; non-destructive, deterministic XorShift seeds).
- **Linux-compat end-to-end → `selftest_lx`** (feature-gated harnesses) and
  the `tools/e2e_*.ps1` scripts (local mini-repo, live apt update, bigindex
  repro).
- A regression fix without a test is not done. New pure module without a
  property is suspicious.

## Conventions

- Doc comments carry the design: ABI contracts, requirement tags (R-numbers),
  SAFETY notes. Match the density of neighboring code; when you change
  behavior, change the comment in the same commit.
- Update the subsystem `README.md` (and root `README.md` limitation lists)
  when behavior changes — stale docs here are treated as bugs.
- Commits: `area: imperative summary` (`kernel:`, `linux:`, `net:`, `fs:`,
  `fix:`, `docs:`, `ci:`), body explains *why* and *what breaks without it*.
- Work on feature branches → PR; merge only with CI green. Stacked PRs are
  fine (branch from the earlier feature branch, then retarget when it
  merges).
- The shell/regression surface is the serial log: diagnostics use
  `[WATCHDOG]`, `[DIAG]`, `LXSELFTEST`, `[EXC #N]` markers — E2E asserts
  grep them; don't rename them casually.

## Versioning

Semver `MAJOR.MINOR.PATCH` — in that standard order (major, *then* minor,
*then* patch). The single source is `Cargo.toml [package].version`; it flows
into `/mnt/etc/pagh-release` and the motd via `env!("CARGO_PKG_VERSION")`
(`provision.rs`), so a stale version is user-visible at boot.

- **MAJOR** — image-level breaks: on-disk/boot format changes, removed
  syscall families, incompatible userland expectations. Rare; last was
  2.0.0 (own TCP/IP stack + e1000 replacing smoltcp + virtio-net).
- **MINOR** — new user-visible features or behavior changes, even
  fully backward-compatible ones. Precedent: `release 1.1.0: the tick-rate
  change is a feature (behavior change), so minor bump, not patch`.
  Land the bump in the same PR (or the final commit of a stacked series)
  that ships the feature — not some day later. Current 2.1.0 = the COW
  fork/demand-paging drop (PR #8) + real POSIX signal delivery (PRs #9/#10).
- **PATCH** — fixes, diagnostics, docs, tooling, vendored-dep refreshes with
  no behavior change. Docs-only commits (this file, READMEs) do not bump.
- **Tags**: tag the release commit with the bare version string (`2.1.0`,
  matching the existing tag style) and push the tag. Tags must never run
  ahead of `Cargo.toml` — the old `2.0.4` tag sat on a commit whose crate
  version was still 2.0.3; that drift is exactly what this section prevents.
- CI does not enforce the bump (yet); reviewers and agents must.

## Known open gaps (good first issues, all documented in-code)

- procfs does not exist (only emulated `/proc/self/exe` readlink).
- Signals: delivery happens at the syscall-return point; no timer-tick
  delivery, no `kill(2)`/group broadcast, no SIGSTOP/SIGCONT scheduling.
- NVMe driver is polled, page-chunked, has no FLUSH (weakens WAL ordering
  guarantees on real hardware).
- embedded-tls deterministically hangs on large streams — live apt update
  runs over plain HTTP because of it.
