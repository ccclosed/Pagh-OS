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
debug library; `tools/build.py build` additionally links, and `tools/build.py stage`
writes the bootable ESP used by QEMU. To see the
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
kernel-build      -> tools/build.py          # builder: cargo + linker; `stage` writes the ESP
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
   `lx_selftest` / `lx_livetest` / `lx_bigindex` / `lx_tlsbig` harnesses must stay
   compiled-out (and boot-unchanged) when unset. TLS is fail-closed: the
   handshake aborts unless the chain validates against the committed CA
   bundle, the SAN authorizes the host, the clock gate passes, and the
   `CertificateVerify` signature checks out. `apt` also verifies what it installs:
   `InRelease`/`Release.gpg` against the pinned Debian keyring, then the `Packages`
   digest **and** size from the signed `Release`, then each `.deb` digest **and**
   size from the signed index; what remains unverified (revocation distribution,
   replay of a whole old triplet on suites without `Valid-Until`, the index-free
   `pkg` command) is named in `SECURITY.md`.
10. **Unsafe policy**: every `unsafe {` in `src/security/`,
    `arch/x86_64/linux/mod.rs`, `memory/vmm.rs`, `net/tls.rs`, `pkg/apt.rs`
    needs a `SAFETY:` comment within the previous 6 lines
    (`tools/check_safety.py` enforces; the rest of the kernel follows the
    same style voluntarily — keep it).
11. **Credentials are syscall-layer state, never VFS policy.**
    `CompatState::creds` (`src/task/compat.rs`) is the only owner of a process's
    six ids; it starts as `Creds::ROOT`, is inherited by `fork`/`clone`, and is
    preserved by `execve` (setuid binaries are not modelled). The pure decisions
    live in `src/arch/x86_64/linux/cred.rs` (host property P56). Invariants:
    * `VfsNode` carries metadata only (`mode`/`owner`/`set_mode`/`set_owner`).
      Never add a credential check inside a `VfsNode` implementation — boot
      provisioning, the package installer and the shell call the VFS directly and
      must keep working without an identity.
    * root bypasses **read and write** but never **execute**: a mode with no
      `0o111` bit is not executable even for root. Do not "fix" a `0644`
      executable by special-casing root.
    * a new syscall follows the three-edit `abi.rs` contract (nr constant, dispatch
      arm, `SUPPORTED_SYSCALLS` + the independent `EXPECTED` literal). Path
      syscalls use `check_node_access`/`check_dir_mutation`; id changes go through
      a `Creds` transition method, never field-by-field in a handler.
    Contract and exact gap list: `docs/security-model.md`.

## Testing philosophy

- **Pure logic → host property tests.** A module must stay `core`(+`alloc`)
-only to be `#[path]`-included in `host-tests/src/lib.rs` and covered by a
  `properties/pNN.rs` property file. If your change touches such a module
  (`abi`, `errno`, `validate`, `io`, `wire`, `deb`, `tar`, `apt_index`,
  `signal_frame`, `cred`, …), extend/adjust the property; the `#[cfg(test)]`
  `supported_set_is_exact` list in `abi.rs` must match `is_supported`.
- **Kernel-internal state → in-QEMU selftests** (`src/test.rs`, run via the
  `selftest` shell command; non-destructive, deterministic XorShift seeds).
  `selftest 2` runs the suite twice in one boot and prints a
  `SELFTEST IDEMPOTENCY: …` verdict plus each pass's
  `[selftest] PMM hygiene: free frames … -> …` ledger: a routine that retains
  PMM frames (or any other kernel state) makes the passes diverge, which is how
  an ELF fuzz routine was found to drain the whole pool.
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
  `[WATCHDOG]`, `[DIAG]`, `LXSELFTEST`, `[EXC #N]`, `[BOOTID]` markers — E2E
  asserts grep them; don't rename them casually. `[BOOTID]` is the boot-identity
  proof (`tools/e2e.py` patches its placeholder inside the staged ELF and
  requires the per-run tag back on serial — see `tools/e2e_boot_integrity.md`);
  removing or reflowing it silently turns every E2E verdict unfalsifiable.

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
  that ships the feature — not some day later. (tags
  2.3.0 → 2.4.0 → 2.4.1 → 2.4.2 → 2.5.0, each with a GitHub release);
  `Cargo.toml` is the source of truth, and
  a feature in flight lands its own MINOR bump in the PR that ships it — 2.4.0
  arrived with the format-guard PR, and a branch that has not merged yet must not
  be reflected here. **`main` currently reads 2.22.0** (the ring-3 isolation
  hardening batch from the 2026-11-13 attack audit, on top of the console
  scrollback and the shell/procfs batches; 2.18.13 carried the `connect(2)`
  reply-and-close fix, the VT/console coordinate fix and the kernel-thread
  rows in `ps`; the tag lands at the end of the batch).
  **2.22.0** = ring-3 isolation hardened after the attack audit. The audit found
  three independent ring3-to-ring0 paths on the stock 2.21.0 guest and one
  one-syscall machine kill; this batch closes all four and adds the regression
  evidence:
  * `mprotect(2)` accepted kernel virtual addresses. `sys_mprotect` had no
    `USER_ADDR_MAX` gate (unlike `mmap`/`brk`), its coverage guard inverted to
    "allowed" for a fully-mapped kernel range, and it re-mapped the found frames
    with `leaf_flags()` — a *user*-page flag builder that always sets
    `USER_ACCESSIBLE`. Shared kernel leaf page tables turned that into a ring-3
    W+X alias of kernel memory; scanning the heap for the process's six `u32`
    credentials and zeroing them gave uid 0 (F-LEAD-1, critical). Fixes:
    `mprotect`/`munmap`/`mremap` share one pure user-range gate (`user_pages`,
    built on `validate::check_user_range`), the `PROT_NONE` frame-free path can
    only free user pages, and `vmm::map` now refuses `USER_ACCESSIBLE` mappings
    of a kernel VA at the single mapping choke point.
  * `rt_sigreturn(2)` restored a user-forged `eflags` verbatim into `r11`, which
    `sysretq` loads as RFLAGS: setting `IOPL=3` gave ring 3 the I/O ports and PCI
    config space (F-LEAD-2, critical). Fixes: `decode_rt_sigframe` rejects
    `ip`/`sp` outside the user half and masks the flags down to
    `USER_RFLAGS_MASK` (CF/PF/AF/ZF/SF/TF/IF/DF/OF; IOPL/NT/VM/RF/AC cleared) —
    the one decode path every restore goes through; the TSS has no I/O bitmap, so
    a future path that raises IOPL still has to change the TSS to reach ports.
  * `recvfrom(2)` wrote the source `sockaddr`/`socklen_t` through raw user
    pointers that were never validated: 16 attacker-influenced bytes plus a
    constant 4 at two arbitrary addresses, proven against the kernel tick counter
    (F-x86a-1, critical — a one-shot arbitrary kernel write). `connect(2)`
    dereferenced the caller's `sockaddr` after checking only its *length*, so a
    non-canonical pointer caused a ring-0 #GP and the fault handler halted the
    machine (F-x86a-3, medium). Both now go through `check_user_ptr` before any
    read/write; the EINVAL syscall diagnostic is rate-limited to one line per
    second, because the exploit loop otherwise flooded serial *and* framebuffer
    with >1000 lines (attacker-triggerable console DoS).
  * Evidence: the audit's own PoCs against the fixed kernel — `kheap` reports
    `mprotected pages=0x0` and `creds pattern not found` (uid stays 1000),
    `iopl` resumes with `RFLAGS=0x202` and dies on the first port access instead
    of reading PCI config, `connect` survives with `EFAULT` instead of halting,
    `recvfrom` returns before the write; baseline runs on the stock kernel still
    `SUCCESS`/PCI-read, so the contrast is the same host, same PoCs, only the
    kernel differs. Host tests cover the flag mask and the user-range boundary.
  * Still open from that audit (next batches, tracked in `SECURITY.md`): the
    `.deb` symlink escape into `/mnt/etc/autorun` (F-x86n-1), crafted-ext2 huge
    symlink sizes (F-x86v-1 / F-rv-1), missing search-permission checks in
    `walk()` (F-x86v-3), the RISC-V userland-runtime findings (F-rt-1…4), and
    the TOCTOU/static items. They are not silently considered fixed.
  **2.21.0** = the console scrolls back. The framebuffer console used to hold
  exactly the rows that fit on screen, so anything printed above the view was
  gone. It now keeps a bounded ring of scrolled-off *rows* — characters plus the
  per-cell colour, the same model the grid already held — and shows them again
  when the view scrolls: the PS/2 wheel (IntelliMouse extension negotiated at
  init: three magic sample rates, then device id 3 and four-byte packets whose
  fourth byte is the axis), PageUp/PageDown for the keyboard, three lines per
  wheel notch. While scrolled, new output keeps the same absolute rows in view
  (`push` moves the offset with the ring), the status strip reports
  `scroll N/M`, and any ordinary key or click returns to the live bottom (Escape
  included). `clear` forgets the history. The view is a repaint from the model,
  not a pixel shift: the live grid is untouched, so the caret and selection
  semantics stay exactly where they were; a transparent console (wallpaper
  active) paints the history view opaque and `theme::repaint` puts the wallpaper
  back on the return to live. One limit is named, not hidden: the wheel works on
  the PS/2 mouse (a USB HID *boot* mouse has no wheel axis in the protocol, so USB
  users scroll with PageUp/PageDown). Evidence: pure ring/view unit tests in
  `drivers::scrollback` (host-tests, properties in the module), and the committed
  `tools/e2e.py scroll` scenario — QMP wheel notches (the HMP monitor has no wheel
  command), PageUp/PageDown, reset-on-key, live-vs-history screenshots that must
  differ, and a wallpaper round-trip; it also caught the `wallpaper` shell command
  never calling `theme::apply` (the opaque-console knob stayed on, so every
  wallpaper was invisible inside the text grid — fixed here).
  **2.20.0** = `/proc/<pid>/maps` stops claiming `00:00 0` for files. The map's
  PT_LOAD segments (image and interpreter) and file-backed `mmap`s now print the
  `st_dev`/`st_ino` and pathname `stat(2)`/`fdinfo` already report for the same
  VFS node, plus the real file offset of a `mmap` (shifted correctly when a
  partial `munmap`/`mprotect` splits a region). `[heap]`, `[stack]` and
  anonymous mappings keep the honest `00:00 0` and no pathname: they have no
  backing file. The kernel still keeps no VMA list, so the map shows exactly
  what it knows (image, interpreter, heap, tracked `mmap`s, stack) and nothing
  invented. Contract: `docs/procfs.md` §14 (DEVIATION-7 narrowed). Evidence:
  `procfs::maps file identity` selftest routine, `properties/procfs_maps.rs`
  and the `lx_selftest` `maps_metadata` fixture (a real guest ELF that maps its
  own file, reads `/proc/self/maps` and writes the dump for the harness).
  **2.19.0** = nine new shell commands, a long listing for `ls` and `-h` units, one batch
  of "ask the machine instead of reading a log":
  * `free` — physical memory total/used/free from the PMM frame counters plus
    the fixed kernel heap from the global allocator's counters, in the busybox
    `free` layout and KiB. `/proc/meminfo` already exposed `MemTotal`/
    `MemFree` to programs, but the shell had no view and meminfo has no heap
    row; `free` reads the same PMM counters, so the two views cannot disagree,
    and the `Heap` row makes heap growth observable (the region is mapped at
    boot, so it always counts as `Mem used` — only the `Heap` row shows how
    much is actually handed out). Truthful limits, not stubs: `shared` and
    `buffers` are zeros because this kernel keeps no shared-memory accounting
    and no block cache, and `Swap` is zero because there is no swap device.
  * `date` — the CMOS RTC wall clock in UTC (ISO date/time, weekday and Unix
    seconds). `uptime` only ever showed monotonic ticks, even though the same
    RTC reader already feeds `clock_gettime(2)` and TLS validation; `date` is
    that clock in the shell, so a transcript can be timestamped. NTP is not
    implemented, and the kernel assumes the RTC keeps UTC everywhere, so a
    machine set to local time shows it labelled `UTC` rather than a guessed
    correction.
  * `ls -l` — long listing: type letter + nine permission bits (with the
    setuid/setgid/sticky markers in the execute slot), link count, owner,
    group, size and name, plus `-> target` for symlinks. Deliberately no
    timestamp column: `VfsNode` has no `mtime`, and a fake `Jan 1 1970` would
    read as real data.
  * `free -h` / `df -h` — the two memory/disk commands in coreutils' human
    units (`992.9 MiB`, `1.0 GiB`), integer-arithmetic rounding to a tenth, so
    the same numbers in a readable form. Unknown flags are rejected instead of
    silently ignored.
  * `chmod OCTAL-MODE PATH…` — set permission bits. The same nine bits plus
    setuid/setgid/sticky that the VFS `set_mode` contract already stores: ext2
    persists them (visible immediately in `ls -l`), while ramfs/`/dev`/`/proc`
    answer "operation not supported" per path and the other paths are still
    tried. Invalid octal (or a mode above `0o7777`) is rejected rather than
    silently masked.
  * `chown UID[:GID] PATH…` — set owner/group, Linux-shaped (`1000:1000`,
    `1000` keeps group, `:1000` keeps owner), numeric ids only because the
    kernel has no user database to resolve names against; a name fails instead
    of silently becoming uid 0. ext2 persists the owner, other filesystems
    answer per path as in `chmod`.
  * `tree [-L N] PATH` — recursive listing, two ASCII spaces of indent per depth
    and a `N directories, M files` trailer. ASCII because the console font has no
    box-drawing glyphs; depth capped (default 4) and every level goes through
    `readdir`, so a symlink loop cannot recurse without end.
  * `head`, `tail`, `wc`, `grep` — inspect a file without dumping it: first or
    last N lines, line/word/byte counts, and the lines containing a pattern
    (`-i` `-n` `-c` `-v`). All four share one streaming walk
    (`shell::texttools::walk`): 256-byte reads, a 4 MiB cap so an endless
    device (`/dev/serial`) cannot wedge the shell, `tail` keeps only N lines,
    `wc`/`grep` keep counters. `head`/`tail` always terminate the last line so
    the next prompt stays on its own row — the same transcript rule as `cat`.
  * `procfs` — the per-thread procfs view. `/proc/<pid>/task/` (and
    `/proc/self/task/`) list one directory per live task of the process's thread
    group — pagh models a thread as a scheduled task registered under its own pid
    with a shared `tgid` — and `/proc/<pid>/task/<tid>/` is **the same VFS object
    as `/proc/<tid>`**, so `stat`, `status` and `cmdline` (plus the rest of the
    pid-directory files) are the per-thread view with Linux's 52-field `stat`,
    `Tgid`/`Pid`/`Threads` and inherited argv; `/proc/self/task` follows the
    reader's own group and is `ENOENT` for a task without `CompatState`. A dead
    pid, a task of another thread group and a non-numeric tid are all `ENOENT`
    (no wildcard). Contract and deviations: `docs/procfs.md` §13; evidence is the
    `procfs::per-thread task directories` selftest routine, the `lx_selftest`
    real-`clone` checks (thread listing and thread-exit shrink) and
    `properties/procfs_task.rs`.
  **2.17.0** = the batch that made pagh run on real hardware, plus three
  long-standing display/network defects:
  * a peer that answers and closes in one burst now completes `connect(2)`: the
    completion test is `ever_established` (the handshake happened), not
    `state == Established`, a window a reply+FIN skips straight through — this
    is what made the clipboard bridge work once and then fail. `tools/e2e.py
    tcp-shot` pins it down with a negative control;
  * the VT emulator and the text console share one coordinate system (cell
    `(col,row)` is `row*CHAR_HEIGHT`, no status-bar offset) and hand the cursor
    over in both directions inside the spawn's `without_interrupts` window, so
    `lxrun` no longer clears the screen black and no longer marches the prompt
    downwards;
  * `ps` lists kernel threads (`[shell]`, `[net]`, `[usb]`, …) and `df` reports
    the bytes a ramfs is holding;
  * the boot path shows a **disk menu** (`src/boot_menu.rs`) of the partitions it
    can see and what each holds; `Enter` mounts, `F` formats only after the device
    name is typed back, and a 20 s timeout takes the non-destructive choice.
    A partition is formatted **only** from this menu; the shell `disk` command
    stays the deliberate whole-device route, and a disk image can travel on the
    boot medium as a RAM disk (`drivers::memdisk`, first `*.img` Limine module) —
    which is what makes a machine with no supported drive usable at all;
  * USB input: `drivers::xhci` drives an xHCI controller itself (polled, in the
    `usb` thread) and `drivers::usb_hid` translates boot-protocol reports into
    the same PS/2 scancode stream every consumer already reads. Controller
    bring-up and the translation are verified; **device enumeration is not** —
    see the input-device gap below;
  * `drivers::earlyfb` — an emergency console that draws straight into the
    Limine framebuffer with no allocator and no locks, so a failure before the
    real console (or a panic) is visible on a machine with no serial port
    instead of leaving a black screen; boot prints its steps and erases them at
    handoff;
  * `tools/mkimage.py` builds both artifacts people actually boot: a GPT+ESP USB
    image and a **BIOS+UEFI** ISO (Limine's own CD loaders), each carrying
    `disk.img`; the ISO is verified in both firmware modes.
  **2.17.1** = the first field report from real hardware. The emergency console
  named the step (`init_apic`) and caught a #GP inside the LAPIC page-table
  walk — a general-protection fault rather than a page fault, which means the
  virtual address computed from the MADT base was non-canonical, i.e. the
  firmware reported a garbage LAPIC address. `vmm::map_mmio` now refuses an
  implausible physical base with a message instead of faulting inside the walker,
  `apic::init` prints the addresses it was given and falls back to the
  architectural `0xFEE00000` instead of panicking, x2APIC-enabled handover is
  detected and switched back to xAPIC, and boot announces each step *before*
  running it so the step that hangs names itself.
  **2.17.2** = the real cause of the field failure. The `#GP` was in `vmm::map`,
  and the addresses firmware reported were correct (`0xfee00000`/`0xfec00000`):
  the page-table walker treated a **huge page** as a page table. Limine maps the
  HHDM with 2 MiB (sometimes 1 GiB) pages, so a walk into the LAPIC's HHDM
  window found a present entry whose HUGE_PAGE bit was set and whose "frame
  address" is the start of a large RAM region, not 512 entries — indexing that
  produced a non-canonical pointer, which is a general-protection fault rather
  than a page fault. QEMU booted only because its HHDM happened not to have a
  huge page at that address. `ensure_next` now takes the entry's level, splits a
  huge entry into a real table (2 MiB → 512 × 4 KiB, 1 GiB → 512 × 2 MiB, flags
  preserved), and `map_mmio` refuses implausible physical bases.
  **2.18.0** = the first fixes driven by a real machine reaching the shell:
  * the LAPIC timer is **measured** against the PIT at boot instead of assuming
    QEMU's 1 GHz bus clock. On real hardware the assumed constant made every tick
    ~60x too long — the shell's clock advanced one second per minute and every
    timeout was stretched the same way;
  * `drivers::partition` reads GPT (and MBR) and publishes each partition as its
    own block device, so pagh can own one partition instead of claiming the whole
    disk; the boot path prefers a partition that already holds pagh's own volume;
  * `logfile` mirrors every kernel log line into `/mnt/pagh-log.txt` — buffered in
    a fixed ring from the first boot message (the logger runs in interrupt and
    panic contexts, where touching a filesystem would deadlock) and drained by a
    `logd` thread once `/mnt` is mounted, so a machine with no serial port leaves a
    readable log on its disk.
  **2.18.1** = the disk menu and the ownership rule. Booting now shows a menu of the
  partitions it can see (`src/boot_menu.rs`, drawn straight into the framebuffer before
  the shell exists) with what each holds: pagh's own volume, an empty region, a
  `FOREIGN` filesystem, or unknown data. `Enter` mounts, `F` formats only after the
  device name is typed back, `Esc` skips, and a 20 s timeout takes the non-destructive
  default; the menu polls USB HID and PS/2 synchronously because it runs before
  interrupts are on. A volume is pagh's own iff it carries pagh's WAL superblock
  (`Ext2Fs::is_pagh_volume`), so the boot path no longer mounts "whatever ext2 it
  finds": ext3/ext4 are refused by their feature words, a foreign ext2 is left
  untouched, and the only device the automatic path may format is the RAM disk from the
  boot medium. Three bugs surfaced while building this: the ext-family superblock magic
  was probed at 1024 instead of 1024+0x38 (`SB_MAGIC_OFFSET`), which made every real
  ext2/ext4 volume classify as `Foreign` while the host test agreed with the mistake;
  the boot path now runs on its own 128 KiB stack because Limine hands over a small one
  (a latent overflow the partition code's extra call depth turned into a page fault on
  BIOS boots); and the boot menu first rendered half its text in Cyrillic, which the
  kernel font does not cover — its UI is ASCII.
  **2.18.2** = USB input that actually works. The xHCI driver had never run its
  enumeration path, and the reason was a chain of four defects: xHCI port
  registers were addressed with a 4-byte stride instead of 16 (so "port 5" was
  really port 2's PORTSC, and QEMU's keyboard is on port 5 of 8); Address Device
  was handed the HDM-virtual address of the input context instead of its physical
  one; SET_CONFIGURATION was never sent, leaving the HID endpoint stalled; and the
  endpoint context put CErr and MaxPacket in the wrong dword and the polling
  interval in raw form, so the interrupt-IN endpoint was configured as ISO OUT with
  a zero packet size. With those fixed, an emulated USB keyboard enumerates,
  reports, and types into both the shell and the boot menu (arrows/Enter/Esc
  included). Two performance fixes came with it: the report poll no longer spins
  50 000 iterations per empty poll, and the boot menu polls the controller once per
  256 spin steps — the old busy-wait could stretch boot into minutes under TCG.
  **2.18.3** = the first field report after the disk menu: the clock still ran
  slow and the USB keyboard/mouse were dead on the real machine. The clock had one
  cause — the LAPIC calibration call site had been left disabled during the BIOS
  stack debugging (fixed in the previous commit). For the input, the driver now
  finds the keyboard whichever xHCI controller it is plugged into (probe every
  controller, count attached ports, start the best), takes ownership from the
  firmware's USB legacy support, and RETRIES ports that have not produced a device
  yet instead of giving up on the first look — USB2 connect needs ~100 ms of
  debounce after the controller reset, which QEMU's instant answer had hidden.
  A device behind a hub is detected and said out loud (hubs are still not
  supported). Because that machine has no serial port, the boot menu and the shell
  banner now print one line that makes the failure visible on screen: controllers
  seen, ports, which port has a device, how many HID devices were configured, and
  how many keyboard/mouse reports arrived.
  **2.18.4** = the field test showed the USB count it was asked for — 2
  controllers, 26 ports, devices on six ports — and **0 HID**: the controller saw
  the keyboard, enumeration stopped somewhere, and the old build could not say
  where. The boot menu now prints one row per port that has a device: PORTSC and
  the enumeration stage it reached (`reset-ok`, `addressed`, `device-descriptor`,
  `no-hid-interface`, `set-configuration`, `configure-endpoint`, …), the shell
  banner carries the last failure, and the `usb` command prints the same plus
  every PORTSC. Enumeration also got two more real-hardware fixes: EP0's max
  packet size is updated from the device descriptor (full-speed devices start at
  8 and the 18-byte read fails otherwise), USB3 ports whose link firmware left
  suspended are brought up with a warm reset when the hot reset times out, and a
  port whose enumeration fails is retried with growing spacing instead of being
  written off after one attempt.
  **2.18.5** = the port-reset signal. The per-port diagnostic from 2.18.4 came
  back with the key line: every port with a device reported `stage=address
  code=0x4` (USB transaction error) and a settled `ccs=1 ped=0 pls=7` — devices
  present, ports never ENABLED, and the address request sent to a device that had
  never left Polling. The driver had been treating the port-reset-change bit (PRC)
  as "reset done"; on real hardware a reset the device does not complete still
  raises PRC while PED stays 0. Port bring-up now waits for PED, and only then
  reads the speed and addresses the device; if a hot reset does not enable the
  port it retries with a warm reset, then with a PP power-cycle, and reports
  `reset-no-enable` with the before/after PORTSC snapshots if all three fail.
  Those snapshots (`pre=`/`post=`) and the attempt counter are now part of the
  on-screen port rows, so the next field report distinguishes "port never
  enabled" from "enabled but the device did not answer".
  **2.18.6** = the port reset never takes effect. The 2.18.5 field photo shows
  `stage=lost-after-reset` with `pre` and `post` PORTSC **identical** (0x6e1:
  connected, powered, Polling, never enabled) — the controller ignores the port
  writes, or the driver reads the wrong register. QEMU cannot distinguish those,
  so the next build puts the ambiguity on screen: the controller scan (both PCI
  controllers, their port counts and attached masks, which one is in use), a raw
  dump of the selected port's four register-set dwords, USBCMD/USBSTS/CONFIG and
  CAPLENGTH, and a write-read-back test of a harmless register that says whether
  ANY write this driver issues reaches the controller.
  **2.18.7** = the port reset that does not stick. The 2.18.6 photo settles two
  questions: MMIO writes reach the controller (`writes=ok`, USBCMD shows RS=1 and
  the command ring works), the port register is where the driver thinks it is
  (PORTSC/PORTPMSC/PORTLI/PORTHLPMC read 0x6e1/0/0/0), and the chosen controller is
  the 26-port Intel PCH one (8086:7a60; the discrete 10de:1ada has nothing
  attached). The reset itself raises PRC and PED — and then loses PED while the
  port falls back to Polling with `pre == post`. So a single PED sighting was
  being accepted as success; the driver now requires a STABLE enabled state
  (PED with link state U0), and only then addresses the device. A failed stability
  check falls through to a warm reset and then to a PP power-cycle with real
  settling delays, a settle delay is inserted after the controller reset, and the
  diagnostics gained the port's USB protocol generation (parsed from the Supported
  Protocol capabilities) and the PORTSC snapshot at the first PED sighting.
  **2.18.8** = the brief enable, and the ports the reset costs. The 2.18.7 photo
  shows what the reset does: `ped_sc=0x00200e03` is a *healthy* high-speed enable
  (PED=1, U0, speed=3), and `post=0x6e1` is the same port back in Polling — the
  controller enables the device and then loses it on every attempt. Two more
  things became visible that the driver was blind to: the Supported Protocol
  capabilities were never read (every port said `proto=usb0`, so the driver could
  not tell a USB2 port — where a keyboard lives — from a USB3 one), and the
  pre-reset scan had a device on port 26 that vanished after `HCRST` and was then
  abandoned by the retry limit. Extended-capability walking now falls back to the
  0x20 area when HCCPARAMS1 reports xECP=0 (which is also where the BIOS/USB-legacy
  handoff starts looking, so a missed handoff cannot hide there); the raw dump
  prints HCIVERSION/HCSPARAMS1/HCCPARAMS1 and the handoff result; the diagnostic
  shows `attached before reset` versus `now` with the lost port list; ports with no
  device are retried forever at a slow rate and power-cycled every eighth retry
  instead of being dropped after twelve.
  **2.18.9** = retries that outlive the settle. Two consequences of the 2.18.8
  work are fixed here: a failed enumeration used to leave its xHCI slot enabled,
  and the poll loop skips any port that has a slot — so a retry could never happen
  again for the ports that got furthest (the driver now sends Disable Slot, clears
  the DCBAA entry and drops the slot's rings, whose DMA pages are freed by a Drop
  impl); and ports are now retried FOREVER with exponential backoff instead of a
  bounded count, because a PCH USB2 PHY settling after `HCRST` can take seconds and
  a retry loop that gives up in the first second would never see it. The settle
  after the controller reset is longer for the same reason, and the power-cycle
  fallback keeps its own pacing. Measured cost of one failing attempt is kept in
  the tens of milliseconds so the post-boot retry does not burn the CPU.
  **2.18.10** = the extended capabilities were never reached. The 2.18.9 photo
  finally shows HCCPARAMS1 = 0x20007FC1, i.e. xECP = 0x2000 dwords = 32 KiB — and
  the capability walk gave up at 4 KiB, so on the field machine the driver had
  NEVER performed the BIOS/USB-legacy handoff (`legacy=absent`) and never saw the
  Supported Protocol entries (`proto=usb0` everywhere). That is one mechanism by
  which a BIOS-owned controller keeps manipulating USB2 port state under SMM, and
  it is exactly the kind of thing that makes a port enable and fall back to
  Polling. The walk limit is now the mapped 64 KiB. Two more observations from the
  same photo are addressed: the controller DOES bring ports up by itself (port 26
  read PED=1, link U0 while the driver was waiting to retry), so a port that is
  already enabled is adopted instead of reset back into Polling; and the PP
  power-cycle fallback now runs only every fourth attempt — it was what made the
  mouse's LED blink once every ~35 s, which the operator reported.
  **2.18.13** = the field photo explains `usb2=17-41 usb3=17-26`. That line was
  not the controller's answer: the Supported Protocol port-info word was read with
  the spec fields swapped. xHCI 1.2 table 7-4 puts PSIC in bits 31:28, the port
  COUNT in 15:8 and the port OFFSET in 7:0 — and the offset is **1-based** (Linux:
  `XHCI_EXT_PORT_OFF`/`XHCI_EXT_PORT_COUNT`, "port count includes the current port
  offset"). The driver read the count from 31:16 and the offset from 15:8 plus one,
  with a QEMU-only fallback that did the same thing in another order. The raw dwords
  in the 2.18.12 photo are clean under the real layout: USB2 `dw2=0x30191001` =
  offset 1, count 16 → **ports 1–16**, USB3 `dw2=0x40000a11` = offset 17, count 10
  → **ports 17–26**, and the legacy handoff capability sits at 0x846c. So on this
  machine every real USB2 port (1,5,6,9,10,11) should have printed `proto=usb2` and
  printed `proto=usb0` instead — a diagnostic bug, not a port bug, and one that made
  the previous three reports harder to read than they had to be. The dump now prints
  the major/minor revision and the parsed offset/count/PSIC, and masks are computed
  from the corrected fields.
  **2.18.12** = the halted controller. The 2.18.11 field photos (the ones the
  previous session never got to read) show `usbcmd=0x00000000 usbsts=0x00000015`
  — HCH=1, the xHCI was **halted** at shell time — while `legacy=os-owned` and every
  USB2 port sat in PLS=Polling with `try=19`. A halted controller ignores port writes
  and freezes PORTSC, which is exactly the "ports never enable" symptom the last
  three reports describe; the 2.18.9 photo of the same machine reads `usbcmd=1`
  with `legacy=absent`, so the halt arrived with the extended-capability work. Three
  real bugs came out of that comparison:
  * `dcbaa` and the scratchpad ARRAY frame were locals in `start_controller`: only
    their physical addresses were stored, and `DmaFrame::drop` (added in 2.18.9)
    returned those pages to the PMM while the controller still owned them. QEMU
    rarely reuses such a page during boot; hardware does, and the first reuse makes
    the controller fetch a garbage device context — a Host Controller Error, which
    halts the controller mid-boot. Both frames are `Controller` fields now (`dcbaa`,
    `scratchpad_array`, `scratchpad_pages`), for the same reason `erst` always was.
  * xHCI was the only DMA driver here that never called `pci::enable_bus_master`.
    The command ring, event ring, DCBAA and scratchpads are all DMA; bus mastering
    off is invisible under QEMU and fatal on hardware. It is enabled before the BAR
    is touched and printed (`pci_cmd=` in the diagnostic).
  * every USB wait was a spin count, i.e. a different wall-clock duration on every
    machine. A USB2 reset needs ~50 ms plus 10 ms recovery (xHCI 1.2 §4.19.1), a
    suspended link needs 20 ms of resume signalling, and the field budgets were a few
    milliseconds of real time — which is why ports "enabled and then fell back to
    Polling". The driver now calibrates the TSC against a 10 ms PIT channel-2
    one-shot in `init_usb` (before the boot menu starts using that PIT channel) and
    gives the port paths real deadlines; the per-port row prints `ms=`, the duration
    the last attempt actually took, so the next photo can separate "needs more time"
    from "the controller refuses".
  The same photos made four smaller things visible, all fixed here: a port that is
  mid-detect (PLS=Polling/RxDetect) is waited for instead of being reset mid-attach
  (that reset was restarting the attach sequence — the PED-then-Polling loop); an
  enabled-but-suspended port (PED=1, PLS=U3, the state the 2.18.10 photo shows right
  after a reset) is resumed with PLS=U0|LWS instead of being reset again; WPR is only
  issued to a SuperSpeed link (it is undefined on USB2, which now gets the PP power
  cycle instead); and `poll()` detects a halted controller, records the registers
  that prove it and restarts it (HCRST + re-point the rings), so a halt can no longer
  masquerade as "no USB device". The Supported Protocol dump now prints **port masks
  plus the raw capability entries**: the old line read
  `usb2=17-41 usb3=17-26`, which is what a parser that reads the wrong DW2 looks like
  (both entries appeared to start at port 17, so every real USB2 port printed
  `proto=usb0`), and the duplicated `xhci proto:` line in the boot menu — visible as
  the same line twice in every field photo — is gone.
  **2.18.11** = the menu that stopped counting. The 2.18.10 field report: the
  20-second countdown never moved, the keyboard and mouse still did not work, and
  the device LEDs blinked even more often. The cause was the driver itself: with
  the extended-capability walk fixed, the handoff ran (`legacy=os-owned` — the
  semaphore was already ours) and every port now really went through the reset
  path, but `poll()` still processed ALL ports in one pass. On hardware a single
  reset attempt costs hundreds of milliseconds of MMIO waits, so the first poll
  blocked for many seconds inside the menu's one-second slice — the countdown
  never got to tick, and the PP cycles of eight ports back to back is what made
  the LEDs blink. `poll()` now initialises exactly ONE port per call, round-robin,
  so the menu keeps drawing between attempts; the wait budgets are cut to fit a
  fraction of a second; and the diagnostic prints the USB2/USB3 port ranges
  (`xhci proto: usb2=1-10 usb3=11-26`), which is what the next field report needs
  to tell a keyboard port from a SuperSpeed one.
  **2.8.0** = the security model: per-process real/effective/saved uid+gid in
  `CompatState`, the `set*id` family, POSIX owner/group/other enforcement on
  `open`/`execve`/directory mutation/`chmod`/`chown`/`kill`/`access` against real
  ext2 `i_mode`/`i_uid`/`i_gid`, creation stamps the caller's ids and mode,
  `stat` reports real metadata, and SMEP is enabled when CPUID reports it.
  Contract: `docs/security-model.md`. **2.7.1** = the
  F-01/F-03/F-04 QA fixes (STAR readback at info, `cat` transcript terminator,
  a claim gate whose `--probe` really tests it). **2.7.0** = the console renders
  **antialiased** text: the
  embedded font asset holds 8-bit *coverage* per pixel (not a 1-bit bitmap) and
  the blitter blends it between the console's background and foreground colour,
  which is what makes the 8×18 Fira Code cell read like the same face in a
  terminal on the host; the first boot no longer stops at a `Y/n` prompt about the
  python download (install it from the shell with `apt install python3`), `nano+`
  takes the mouse (click to place the caret, drag to select, click a sidebar row
  to switch files) with `^A`/`^C`/`^X` acting on the selection, an arrow key now
  drops a selection, and the software cursor is drawn into the same frame it is
  flushed with, so it no longer flickers while it moves. **2.6.0** = the console
  typeface is Fira Code
  (8×18 cell, rasterised offline by `tools/gen_font.py` into
  `assets/font8x18.bin`; the kernel still ships no TrueType parser), the glyph
  geometry has exactly one source (`framebuffer::CHAR_WIDTH`/`CHAR_HEIGHT`,
  used by the VT emulator, the shell's mouse-to-cell mapping and `paint`), and
  the software mouse cursor is back: it is drawn as part of the frame's shadow
  buffer and flushed with it (the shadow-buffer commit had left it drawn *after*
  the flush, so it never reached the device), while an idle shell skips the
  overlay/repaint cycle instead of copying the surface once per timer tick.
  **2.16.1** = the fastfetch/persistent-disk fixes (mprotect on untracked
  resident pages, COW faults on stack/thread/heap pages, boot-demo truncate);
  **2.16.0** = the 2.16 batch: signalfd/signalfd4 and real process
  groups (`pgid` in `CompatState`, setpgid/getpgid/getpgrp/setsid, group kill),
  tty foreground job control (`TIOCGPGRP`/`TIOCSPGRP`, SIGTTIN/TTOU, Ctrl+Z),
  `/proc/<pid>/fdinfo` + `/proc/<pid>/limits`, and the NVMe rewrite (multi-page
  PRP lists, a four-command polled queue and the bring-up/protocol fixes that
  made NVMe usable under QEMU);
  **2.15.0** = the 15-commit Linux-compat batch: `*at` family, sendfile/
  copy_file_range, legacy/advisory syscalls, memfd_create, timerfd (poll/epoll
  integrated), SIGCHLD delivery, `/proc/*/mountinfo` and shell df/ps, plus the
  incremental selection repaint that fixes the large-drag FPS drop;
  **2.14.0** = shell `df` (real ext2 capacity from `fs_stat`, `-` for synthetic
  filesystems) and `ps` (PID/PPID/STAT/COMMAND rendered from `/proc/<pid>`),
  plus `/proc/self/cgroup` + `/proc/<pid>/cgroup` (`0::/`, no controllers), the
  Linux compat syscall batches, and SIGCHLD delivery to the parent;
  **2.13.0** = `/proc/self/fd` and `/proc/<pid>/fd`: numeric symlinks over the
  live FdTable, with `/dev/serial`, real file/directory paths, `pipe:[...]`,
  `socket:[...]` and `anon_inode:[...]` targets; plus real 1/5/15-minute
  exponentially decayed `/proc/loadavg` (fixed point 16.16, lazy update, no
  FPU), replacing the honest `0.00` placeholder; **2.12.0** = `/proc/mounts` +
  `/proc/self/mounts` backed by a real VFS mount registry (`mount_at_named`
  records rootfs/devtmpfs/ramfs/ext2/proc);
  **2.11.0** = procfs slice 4: `/proc/<pid>/{cmdline,stat,status,maps}` for every live
  pid plus `/proc/self/{stat,statm}`, with target-state snapshots taken under
  `COMPAT_STATES` and formatted after the lock is released; **2.10.0** = the procfs slice 3: `/proc/version` (`pagh <version> (x86_64) #1 SMP`,
  the real crate version, not a fake Linux release), `/proc/filesystems` (only the
  implemented `proc`/`ramfs`/`ext2` drivers), `/proc/loadavg` (currently real 1/5/15-minute
  averages from 2.13.0, plus live runnable/total scheduler counts and the last allocated pid) and
  read-only `/proc/sys/kernel/{ostype,osrelease,hostname}`; host properties and
  in-QEMU checks. **2.9.1** = the memory-docs truth pass: the fixed 512 MiB heap and the
  `MAX_INDEX_*` clean-failure policy are recorded with the exact constants,
  `fork_user_space_cow`'s active-CR3/TLB contract is a debug assertion rather
  than a comment, and the stale layout.rs `dead_code` note is gone (migration
  complete); **2.9.0** = the procfs slice 2: `/proc/stat` with real user/system/idle tick
  accounting in the timer path, host properties and an in-QEMU two-read
  monotonicity check; **2.5.0** = procfs, signals, the verified apt trust chain
  and the durability fixes (issues #11–#19, #32–#35). Every release owes both a
  tag and a GitHub release: `main` reached 2.4.0 without either, so the tag list
  and the release list drifted three minors apart before anyone noticed — an
  unreleased version is invisible to anyone reading the repository from GitHub.
  **Previous documented minor, 2.3.0** = fail-closed
  TLS server authentication (issue #14 series, PRs #22–#30): chain to the
  committed CA bundle, SAN authorization, the 2025 clock gate and
  `CertificateVerify`, plus the certificate-omission bypass closed in the
  vendored `embedded-tls`. Previous minors: 2.2.0 = low-RAM heap cap +
  version-agnostic Limine loader; 2.1.0 = the COW fork/demand-paging drop
  (PR #8) + real POSIX signal delivery (PRs #9/#10).
- **Release cadence (human request, 2.15.0)**: do not tag every commit. Accumulate
  **15 commits per release**; the first commit of the batch carries the version
  bump, the last one carries the tag and GitHub release. Choose the batch version
  by its highest-impact change: any image/ABI break -> MAJOR, any user-visible
  feature -> MINOR, otherwise PATCH. Low-impact documentation and test commits ride in the next
  MINOR/PATCH instead of getting their own release. CI still runs on every commit.
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


- procfs covers slices 1–5 (issue #11, contract `docs/procfs.md`):
  `/proc/{cpuinfo,meminfo,uptime,stat,version,filesystems,loadavg,mounts}`,
  `/proc/<pid>/{cmdline,stat,status,maps,cgroup,mountinfo,limits}`,
  `/proc/self/{exe,cmdline,status,maps,stat,statm,mounts,cgroup,mountinfo,limits}`
  and read-only `/proc/sys/kernel/{ostype,osrelease,hostname}` are rendered from
  live kernel state (CPUID, PMM counters, the tick clock, the scheduler's
  user/system/idle tick accounting and live task counts, and the target
  process's `CompatState` snapshot). `/proc/self/fd` and `/proc/<pid>/fd` expose
  the fd table as numeric symlinks; `/proc/<pid>/fdinfo/<n>` reports the real
  `pos`/`flags`/`mnt_id`/`ino` of an open descriptor (0 for objects with no VFS
  mount/inode), and `/proc/<pid>/limits` renders the 16 Linux rlimit rows from
  the target's override table plus kernel defaults. Deferred: `/proc/<pid>/task/*`,
  mountinfo optional fields and statfs metadata.
- Signals (issue #12): `kill(2)` (nr 62), process-group addressing
  (`kill(0|-1|-pgid)`), `SIGSTOP`/`SIGCONT` scheduling, `wait4(WUNTRACED|WCONTINUED)`
  and timer-tick delivery to a syscall-free task are all implemented. SIGCHLD is
  queued to the parent on child exit/stop/continue (2.15.0). Real process groups
  now exist in `CompatState` (`pgid`, `setpgid`/`getpgid`/`getpgrp`/`setsid`,
  `kill(0)`/`kill(-pgid)` address the group) as of 2.16.0. Terminal job control
  landed with them: the console is a controlling terminal with one foreground
  process group (`drivers::tty`, pure rules in `arch/x86_64/linux/tty.rs`,
  host property `tty_job`), `TIOCGPGRP`/`TIOCSPGRP` answer as
  `tcgetpgrp`/`tcsetpgrp`, a background read gets `SIGTTIN` and (under `TOSTOP`)
  a background write gets `SIGTTOU`, and `^Z` stops the foreground GROUP.
  Deviations that remain, stated in that module rather than implied: no
  sessions (the `tcsetpgrp` session rule is approximated), a background access
  returns `EINTR` instead of being restarted after `SIGCONT`, and `TOSTOP`
  defaults ON where Linux defaults it off.

- Input devices: PS/2 keyboard/mouse (IRQ1/IRQ12) and USB HID through an xHCI
  controller the kernel drives itself (`drivers::xhci`, `drivers::usb_hid`, polled
  from the `usb` thread and directly from the boot menu). The whole path is
  verified in QEMU with an emulated controller and keyboard: port detection, Enable
  Slot, Address Device, descriptors, SET_CONFIGURATION, boot protocol, interrupt-IN
  reports, and typed text reaching both the shell and the boot menu. Four real bugs
  sat between "code written" and "code runs", all fixed in 2.18.2 — see the release
  note. **Not verified: real hardware** (the next field test runs it); not
  implemented anywhere: hubs (root ports only), isochronous transfers, slot
  teardown on unplug, and USB mass storage — which is why the boot medium's disk
  image is mounted as a RAM disk rather than read from the stick.

- NVMe driver is polled (no IRQs) with multi-page PRP lists and four commands in
  flight; FLUSH (opcode 0x00) is issued at WAL transaction boundaries. Still
  missing: the driver attaches the first active namespace only (partitions are
  sliced by `drivers::partition` into `nvme0pN`), one I/O queue pair (admin commands are serialized), no timeout
  recovery (a completion timeout marks the device failed until reboot), no
  SMART/self-test or shutdown notification, and unaligned writes are
  read-modify-write per LBA rather than batched.
  (The former "embedded-tls hangs on large streams" gap is closed: a multi-MB
  TLS fetch completes end to end — `lx_tlsbig`, issue #19 — and the live
  `apt update` runs over HTTPS.)

- Process credentials + filesystem permissions are implemented (contract
  `docs/security-model.md`): per-process real/effective/saved uid+gid in
  `CompatState`, the `set*id` family, owner/group/other checks on `open`,
  `execve`, directory mutation, `chmod`/`chown` and `kill`, and ext2
  `i_mode`/`i_uid`/`i_gid` write-back. What is still missing and named in
  `SECURITY.md`: no login/accounts (the shell and all kernel-spawned processes
  start as root), no setuid-binary elevation on `execve`, no supplementary
  groups/ACLs/`fsuid`, no capability sets/seccomp/LSM/audit, no sticky-bit or
  directory-owner checks, synthetic nodes (`/dev`, `/proc`, ramfs) cannot
  persist an owner and keep the historical per-process `stat` override, and
  SMEP is the only hardware mitigation (SMAP/KPTI are absent). `access(2)`
  checks real ids; `stat` reports real inode metadata.
