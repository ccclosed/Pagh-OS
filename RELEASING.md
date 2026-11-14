# Releasing Pagh-OS

This is the operational runbook for `Pagh-OS`. The branch model is in
`BRANCHING.md`; the version/feature rules are in `AGENTS.md` / `README.md`.
Nothing here releases automatically except the tag-driven `release` workflow.

## Version source

`Cargo.toml` (`[package].version`). It is what the running system reports via
`env!("CARGO_PKG_VERSION")` (`src/provision.rs` writes `/mnt/etc/pagh-release`
and the motd), so a stale version is user-visible at boot — and the workflow
refuses a tag whose base version disagrees with it.

`dev` carries the **next release** version; `main` keeps the last **released**
version. The first feature after a release bumps MINOR, the first fix bumps
PATCH. The `AGENTS.md` "`main` currently reads" line describes the latest
**stable** release only: an in-flight bump on `dev` does not rewrite it, and it
is updated together with the stable release.

## Required checks and live evidence

A release is cut only from a commit where **all four required CI jobs** are
green (`.github/workflows/ci.yml`):

| Job | What it proves |
|---|---|
| `kernel` | `cargo fmt --check` and the debug + release kernel builds |
| `host-tests` | the `host-tests/` property/regression suite |
| `static-policy` | no unreviewed `unsafe` blocks (`tools/check_safety.py`) |
| `docs-claims` | the verifiable claims in `AGENTS.md` match the tree |

On top of CI, collect the live evidence on the exact commit that will be
tagged:

- `python3 tools/e2e.py selftest` — boots the staged release kernel in QEMU and
  runs the in-guest `selftest`; the run is only valid with its `[BOOTID]` boot
  identity proof and `SELFTEST SUMMARY` (see `tools/e2e_boot_integrity.md`).
- The committed security-regression PoCs still behave as documented:
  `tools/security/poc/poc_poll_toctou.c` (user-buffer TOCTOU class),
  `tools/security/poc/poc_sock_large_count.c`, the round-1 PoCs, and
  `tests/pkg/e2e_pkg_escape.py` (crafted `.deb` packages refused, no
  `/mnt/etc/autorun`). A released kernel must not regress a closed finding.

## Pre-release (release candidate) — from `dev` only

1. Freeze `dev`: after `vX.Y.Z-rc.1`, only fixes for this release are merged
   until the release lands. Everything else waits.
2. Pre-flight on the exact `dev` commit:
   - the four required checks green;
   - the live evidence above collected;
   - `CHANGELOG.md` section written (and the release-note block in `AGENTS.md`
     updated);
   - version source equals `X.Y.Z` (no `-rc` in the source file);
   - no open blocker, no known regression in the release scope.
3. Tag and push `vX.Y.Z-rc.N`:
   ```sh
   git checkout dev && git pull --ff-only
   git tag -a vX.Y.Z-rc.1 -m "pagh X.Y.Z-rc.1"
   git push origin vX.Y.Z-rc.1
   ```
4. Watch the `release` workflow: it refuses the tag unless it is reachable
   from `dev` **and not** from `main`, checks the base version, builds the
   release kernel, writes `.sha256` files, and publishes a GitHub
   **Pre-release**.
   A rejected tag stays in the repository but produces **no** release entry:
   delete it (`git push origin :refs/tags/<tag>`) before retrying.
5. Test the pre-release artifact. Bugs: fix on `dev`, tag `-rc.2` (never move
   the `-rc.1` tag).

## Stable release — from `main`

1. Merge `dev` into `main` (merge commit; no code changes at this step):
   ```sh
   git checkout main && git pull --ff-only
   git merge --no-ff dev
   git push origin main
   ```
2. Wait for CI on `main` to be green.
3. Tag and push `vX.Y.Z` on the merge commit; the workflow checks that the
   commit is reachable from `main` and that the version source matches.
4. Verify in **Releases**: the new entry is a full release with the versioned
   kernel artifact and matching `.sha256` (see *Artifacts* below). GitHub marks
   the newest stable as **Latest**.
5. `dev` continues with the next version (first change bumps it).

## Hotfix — straight to `main`, no RC

Hotfixes are limited to **critical/high verified security findings** or a
release-blocking regression in a released version. Everything else rides `dev`.

1. Branch from `main`: `hotfix/<short-name>`.
2. PR to `main` (never to `dev`), full CI plus a targeted regression test/PoC
   for the bug. Get it reviewed.
3. Merge to `main`, then tag the PATCH release `vX.Y.Z+1` directly (no RC) and
   verify the release entry.
4. Merge `main` back into `dev` with `--no-ff`:
   ```sh
   git checkout dev && git pull --ff-only
   git merge --no-ff main
   ```
   Resolve version conflicts in favour of the larger version (dev stays ahead).
   `CHANGELOG.md` and `AGENTS.md` merge additively: keep both release sections.
5. CI on `dev` must be green after the merge-back.

## Artifacts

Every release entry carries (built by `.github/workflows/release.yml`):

| File | Contents |
|---|---|
| `pagh-vX.Y.Z.elf` (or `pagh-vX.Y.Z-rc.N.elf`) | the release kernel ELF |
| `pagh-vX.Y.Z.elf.sha256` | its SHA-256 checkout |
| `Pagh-OS-vX.Y.Z.tar.gz` | source archive (`git archive`, tag-prefixed) |
| `Pagh-OS-vX.Y.Z.tar.gz.sha256` | its SHA-256 checksum |

Verify before using an artifact:

```sh
sha256sum -c pagh-vX.Y.Z.elf.sha256
```

## Cross-repository ABI change order

When a change touches the C/ABI boundary, release in dependency order and only
then update dependants:

1. `pagh-api` / `pagh-libc` (whichever changed; if both, `pagh-api` first when
   the SDK pins the libc header).
2. `pagh-rust` / `pagh-python` (they consume `pagh-api`/`pagh-libc`).
3. `pagh-programs` (apps built against the new SDK), `pagh-repo` (published
   packages), and finally Pagh-OS / pagh-riscv images that ship the new
   runtime/ABI.
4. Update the pinned sibling versions (`Cargo.toml` path/registry deps,
   `Cargo.lock`) in the same release train, not later.

Never tag a dependant before its dependency is tagged; the release workflow
only checks branch/version, not cross-repo compatibility.