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
  be reflected here. **`main` currently reads 2.18.8** (the batch open with the
  `connect(2)` reply-and-close fix, the VT/console coordinate fix and the
  kernel-thread rows in `ps`; the tag lands at the end of the batch).
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
