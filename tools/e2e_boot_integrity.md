# Boot identity: how `tools/e2e.py` proves which image the guest executed

**Scope:** verification integrity (t1 infrastructure). Applies to every mode of
`tools/e2e.py`; the kernel half ships as `tools/e2e_bootid_kernel.patch`
(applied by t27).

## The gap this closes

`summary.json` recorded `elf_sha256` — the hash of the **built** artifact. Nothing
proved the guest executed it. A kernel booted from any other device prints the
same `LXSELFTEST` / `SELFTEST SUMMARY` markers, so a green verdict on a foreign
image was indistinguishable from success: "we verified what we built" was an
assumption, not evidence. Two concrete mechanisms made that reachable:

1. **Shared NVRAM.** `OVMF_VARS.fd` is guest-writable state (boot entries,
   `BootOrder`) and was reused across runs, so boot state from run *N* steered
   run *N+1*.
2. **No boot proof at all.** Even with a fresh NVRAM, nothing in the log tied the
   output to the staged bytes.

Note on a rejected idea: a firmware-log heuristic ("BDS started a HARDDISK ⇒ wrong
device") was implemented, tested, and **removed** — QEMU's `-drive
file=fat:rw:<stage>` is itself an ATA device that OVMF logs as `UEFI QEMU
HARDDISK`, so the pattern fires on *successful* boots too. It was replaced by the
strict marker below.

## Layer 1 — per-run NVRAM (prevention)

`resolve_ovmf(requested, vars_dest)` now always populates a **private per-run**
file `.cache/e2e_run_<mode>_<pid>/OVMF_VARS.fd`:

* template precedence: `$PAGH_E2E_VARS_TEMPLATE` (test hook) → pristine system
  `OVMF_VARS.fd` → legacy shared `.cache/OVMF_VARS.fd` (fallback, warns because it
  may carry accumulated NVRAM);
* the per-run file is rewritten from the template on every run and deleted in
  `cleanup()`; `sweep_stale_run_dirs()` removes leftovers of killed runs.

## Layer 2 — `[BOOTID]` (proof)

Kernel half (`tools/e2e_bootid_kernel.patch`, 2 lines + comment):

```rust
pub const BOOT_ID: &str = "PAGH-BOOTID-0000000000000000";   // fixed length
// in init_serial(), right after `info!("serial")`:
info!("[BOOTID] {}", BOOT_ID);
```

Driver half:

1. **Patch (staged copy only).** After staging, `patch_boot_tag()` replaces the
   placeholder inside `iso_root/pagh.elf` with a per-run tag
   `PAGH-BOOTID-<12 hex of the built ELF's sha256><4 digits of pid>`. The build
   artifact is never touched.
   *Length-preserving:* image size is asserted unchanged, and every differing byte
   must fall inside the 28-byte marker window (the shared `PAGH-BOOTID-` prefix is
   unchanged, so 15–16 bytes differ in practice).
   *PT_LOAD-aware:* a byte pattern can also live in a section Limine never maps
   (`.symtab`, `.debug_str`), and patching such an occurrence would leave the
   running kernel printing the unpatched constant — a false refusal. The patch
   therefore requires **exactly one occurrence inside a `PT_LOAD` segment**
   (`_elf_load_ranges`); zero or ≥2 such occurrences ⇒ no patch, reported as
   unverifiable. Offsets, size and both sha256 values land in `summary.boot_patch`.
2. **Assert.** After the run, `boot_integrity()` classifies the serial log:
   * `verified` — the per-run tag came back: the guest executed the staged image;
   * `fail` — a *different* tag came back, or no tag while the staged image had
     one, or no kernel banner at all: `BootIdentityError` ⇒ verdict
     `REFUSED (boot identity)`, **exit 2**;
   * `unverifiable` — the staged kernel carries no placeholder: recorded as
     `unverified` (never green) in checks + `summary.boot`, plus a loud warning;
     with `--require-boot-proof` it becomes a failure instead.
3. **Summary.** `summary.boot` = `{marker, tag_expected, tags_seen,
   kernel_banner_seen, verified, level, detail}`; `summary.boot_patch` = the patch
   audit record. A reader can therefore see, per run, whether identity was proven.

## Layer 3 — cross-worktree lock (correlated infra fix)

`iso_root`/`disk.img` are per-worktree, but the **mirror port (8000)** and the
QEMU user-net forward are per-host: two runs in different worktrees collided with
a cryptic `QEMU exited before the shell prompt`. The lock now defaults to
`/tmp/pagh-e2e.lock` (`--lock-file`, `$PAGH_E2E_LOCK`), so runs serialize across
worktrees, and a busy port reports it explicitly instead of masquerading as a
guest crash.

## Tests run for this change (all green)

Offline (`python3 /tmp/int_offline.py`, table copied to the PR/report):

| case | expected |
|---|---|
| `boot-check` with the matching tag | exit 0 `verified` |
| `boot-check` with a foreign tag | exit 1 `fail` |
| `boot-check` with no tag while staged image had one | exit 1 `fail` |
| `boot-check` on a log without a kernel banner | exit 1 `fail` |
| `boot-check` without `--tag` (kernel half absent) | exit 3 `unverifiable` |
| patch real kernel ELF | patched inside `PT_LOAD`, size kept, sha256 changed |
| synthetic ELF: 1 loaded + 1 debug-only occurrence | patches the loaded one |
| synthetic ELF: 2 loaded occurrences | refuses (ambiguous) |
| real kernel without placeholder | `unverifiable`, reason names the patch file |

Live (`tools/e2e.py selftest`, per-run serial logs):

| scenario | outcome |
|---|---|
| **kernel with `[BOOTID]` + real foreign boot in NVRAM** (`PAGH_E2E_VARS_TEMPLATE` = VARS poisoned by booting a Limine-only FAT image) + `--require-boot-proof` | `PASS`, `boot identity: verified`, tag from serial matches the staged tag; poisoned NVRAM did **not** produce a PASS on the foreign image |
| real foreign boot log (Limine without our kernel), `boot-check` | exit 1, `fail`: "no kernel boot banner on serial" |
| kernel without the patch, plain run | `PASS` **plus** `WARN … unverifiable`, `summary.boot.level = "unverifiable"`, last check status `unverified` |
| kernel without the patch + `--require-boot-proof` | `REFUSED (boot identity)`, **exit 2** |

## Follow-up (do not forget on integration)

Once `tools/e2e_bootid_kernel.patch` is in `main` (t27 applies it), flip the
default **in the same PR that lands the patch**: add `default=True` to
`--require-boot-proof` and keep an explicit `--allow-unverified-boot` escape hatch
for one-off debugging. Until then the compatibility path is intentional: the
driver must not paint existing runs red before the kernel half exists.

## Note for reviewers

`tools/e2e.py` appears as a **new file** in this branch: the Linux E2E driver
(port of the `tools/e2e_*.ps1` harnesses) was untracked in `main` (t1 legacy).
This branch adds it wholesale together with the boot-identity work — the diff is
not "1400 lines of new infrastructure invented here", it is the t1 driver plus
this change.
