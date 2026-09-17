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

## Commands (all six must be green before you push)

```sh
cargo fmt --all -- --check                  # formatting gate
python3 tools/build.py build                # debug kernel + link (CI parity)
python3 tools/build.py build --release      # release kernel + link
python3 tools/host_tests.py                 # host property tests (or: cd host-tests && cargo test)
python3 tools/check_safety.py               # unsafe-policy gate
python3 tools/check_agents_md.py            # the claims in this file vs the tree
```

CI (`.github/workflows/ci.yml`) runs exactly these six, one job each. CI is the
arbiter — local green is necessary, not sufficient. The block is a *set*, not a
suggestion: `tools/check_agents_md.py` compares it with the `run:` steps of the
workflow in both directions, so a gate that CI runs and this block omits (or the
other way round) fails. For a faster inner loop, `cargo build` alone builds the
debug library; `tools/build.py build` additionally links and stages. To see the
claim gate prove itself, run it with `--probe`: it breaks each class of claim in
a scratch copy and requires a finding for every break.

The kernel needs the pinned nightly (`rust-toolchain.toml`) with `rust-src`
(build-std) and `rust-lld`; it links via `linker.ld` into `pagh.elf`. On
Windows use `run.cmd build|run`; on Linux `./build.sh` / `./run.sh`.
`OVMF.fd` and `disk.img` are local, git-ignored — never
commit them. The Limine loader is version-agnostic: any `limine*/` tree is
git-ignored; `tools/limine.py` finds it (or auto-downloads the latest binary
release into `limine/`) — do not hard-code Limine versions in scripts.

## Canonical entry points

One role, one current path — the one CI, the docs and a fresh checkout should
use. A `-legacy` companion is kept for parity on another OS and is **not** run
here. Which row is current is a statement about the repository, not something a
gate can infer from the filesystem — so it is written down, and
`tools/check_agents_md.py` checks the table: every path exists, a `-legacy` row
has its canonical counterpart, names a different file and says why it is legacy.

```text
kernel-build      -> tools/build.py          # builder: cargo + linker + stage (debug/release)
kernel-run        -> run.sh                  # Linux/QEMU launcher (build.sh = toolchain guard)
kernel-run-legacy -> run.cmd                 # Windows-only legacy launcher (run.cmd build|run)
kernel-e2e        -> tools/e2e.py            # in-guest E2E driver (Linux/CI, no pwsh)
kernel-e2e-legacy -> tools/e2e_*.ps1         # Windows-only legacy harnesses (need pwsh), same scenarios
```

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
  - **One deliberate local patch: `vendor/embedded-tls/src/connection.rs`.**
    Upstream reached `ApplicationData` even when the server never sent
    `Certificate`/`CertificateVerify`, so a peer (or an active MITM
    terminating the key exchange itself) could skip the verifier entirely.
    `Handshake` now carries `certificate_received`/`certificate_verified` and
    `Finished` is refused unless both are set (PSK handshakes excepted). If you
    ever refresh that crate, re-apply this and update the entry for
    `src/connection.rs` in `vendor/embedded-tls/.cargo-checksum.json` — cargo
    fails the build otherwise. `src/net/tls.rs::VERIFIED_HANDSHAKES` is the
    kernel-side belt to that braces.

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
5. **`ticks()` advances only with interrupts enabled.** A real syscall enters
   with IF masked (SFMASK / interrupt gate) and `linux_dispatch` unmasks it for
   the handler's duration — but only when the caller passes a non-zero
   `reentry_allowed`, which the two entry stubs do and the boot-time selftest
   (a direct call on the non-schedulable boot thread) must not. A `hlt`-based
   sleep with IF masked sleeps forever.
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
   compiled-out (and boot-unchanged) when unset. TLS is fail-closed: the
   handshake aborts unless the chain validates against the committed CA
   bundle, the SAN authorizes the host, the clock gate passes, and the
   `CertificateVerify` signature checks out; repository metadata signatures
   are still unverified — see `SECURITY.md`.
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
- **Linux-compat end-to-end → `selftest_lx`** (feature-gated harnesses) plus the
  in-guest driver `tools/e2e.py` (modes `selftest`, `local-mirror`, `live-update`,
  `bigindex`, `shell`): local mini-repo, live apt update, bigindex repro,
  multi-MB TLS stream via `lx_tlsbig`. The `tools/e2e_*.ps1` harnesses are the
  Windows-only legacy equivalents of the same scenarios — see *Canonical entry
  points*; new work goes into `tools/e2e.py` (Linux and CI can run it).
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
- **Immediately after a PR merges, delete its branch (remote and local) and
  retarget any stacked PRs to `main`.** A leftover merged branch is a trap:
  the next stacked PR still defaults to it as base, and the merge then lands
  on the branch instead of `main` — main silently misses the work while the
  branch looks done (this is exactly how PR #27 initially bypassed `main`).
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
  that ships the feature — not some day later. **`main` currently reads 2.4.2**
  (tags 2.3.0 → 2.4.0 → 2.4.1 → 2.4.2); `Cargo.toml` is the source of truth, and
  a feature in flight lands its own MINOR bump in the PR that ships it — 2.4.0
  arrived with the format-guard PR, and a branch that has not merged yet must not
  be reflected here. **Last documented minor, 2.3.0** = fail-closed
  TLS server authentication (issue #14 series, PRs #22–#30): chain to the
  committed CA bundle, SAN authorization, the 2025 clock gate and
  `CertificateVerify`, plus the certificate-omission bypass closed in the
  vendored `embedded-tls`. Previous minors: 2.2.0 = low-RAM heap cap +
  version-agnostic Limine loader; 2.1.0 = the COW fork/demand-paging drop
  (PR #8) + real POSIX signal delivery (PRs #9/#10).
- **PATCH** — fixes, diagnostics, docs, tooling, vendored-dep refreshes with
  no behavior change. Docs-only commits (this file, READMEs) do not bump.
- **Tags**: tag the release commit with the bare version string (`2.1.0`,
  matching the existing tag style) and push the tag. Tags must never run
  ahead of `Cargo.toml` — the old `2.0.4` tag sat on a commit whose crate
  version was still 2.0.3; that drift is exactly what this section prevents.
- **The bump must include the `Cargo.lock` `pagh` entry.** CI builds with
  `cargo build --locked`, so a version bump without the lockfile update
  fails the kernel job in ~20 s. A local `cargo build` refreshes the lock —
  commit it in the SAME commit as `Cargo.toml`.
- CI does not enforce the bump (yet); reviewers and agents must.

## Known open gaps (good first issues, all documented in-code)

- procfs covers its first slice only (issue #11, contract `docs/procfs.md`):
  `/proc/{cpuinfo,meminfo,uptime}` and `/proc/self/{exe,cmdline,status,maps}` are
  rendered from live kernel state (CPUID, PMM counters, the tick clock, the calling
  process's `CompatState`), and `/proc/self/exe` is a real symlink for
  `readlink`/`lstat`/`getdents64`. Deferred, and returning `ENOENT` today:
  `/proc/<pid>` enumeration, `/proc/stat` (needs real user/sys/idle tick
  accounting), `/proc/loadavg`, `/proc/mounts`, `/proc/self/{fd,stat,statm}` — so
  `htop`/`ps` are still out of reach.
- Signals: delivery happens at the syscall-return point; no timer-tick
  delivery, no `kill(2)`/group broadcast, no SIGSTOP/SIGCONT scheduling.
- NVMe driver is polled and page-chunked; FLUSH (opcode 0x08) is issued at WAL
  transaction boundaries, but there are no PRP lists and no queue depth > 1.
- embedded-tls deterministically hangs on large streams — live apt update
  runs over plain HTTP because of it.
