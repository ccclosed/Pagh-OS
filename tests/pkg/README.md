# `tests/pkg/` — package-installer security fixtures

Regression material for **F-x86n-1** (2026-11-13 attack audit, high/verified): a
`.deb` could carry a symlink member that redirects later archive members outside
the installation root, ending in `/mnt/etc/autorun` — executed as root at the next
boot — or at the install root itself (`/mnt/pwned_root`). The fix lives in
`src/pkg/install.rs` (pure planner) and `src/pkg/install_fs.rs` (confined
resolver); its contract is described in `SECURITY.md`, `AGENTS.md` 2.22.1 and
`EXT2-LINKS.md` §5.6.

It also carries the round-2 follow-up **F-r2-vfs-1** (2026-11-14, high/verified):
a hard-link member whose target is an archive symlink is a second name for the
*symlink inode* (`link(2)` does not follow the final component), so
`./d/y -> ../etc` + `./d/h` HARDLINK to `./d/y` + `./d/h/autorun` HARDLINK to the
payload wrote `/mnt/etc/autorun` again. The fix adds the hard-link/symlink
equivalence closure to the planner and a no-alias rule to the effectful
resolver; contract in `SECURITY.md`, `AGENTS.md` 2.22.5 and `EXT2-LINKS.md` §5.6.

Nothing here is compiled into the kernel, installed by the build or run by CI.

## Files

| File | Purpose |
|---|---|
| `craft_evil_deb.py` | Reproducible generator for the audit's hostile `.deb` files (deterministic bytes, mtime 0) plus a positive control |
| `e2e_pkg_escape.py` | Live QEMU proof: serves the crafted packages over HTTP, runs the guest's own `pkg` downloader and asserts on the serial log |

## What the crafted packages are

| Package | `data.tar` members | Expected behaviour on a fixed kernel |
|---|---|---|
| `evil-autorun.deb` | `./y -> /etc`, `./y/autorun` | whole package refused (`UnsafePath`, path `/mnt/y/autorun`); `/mnt/etc/autorun` never created |
| `evil-symlink.deb` | `./x -> /`, `./x/pwned_root`, `./usr/bin/evil-ok` | refused (`/mnt/x/pwned_root`); no write at the install root |
| `evil-relative.deb` | `./z -> ../../..`, `./z/escape` | refused (`/mnt/z/escape`) |
| `evil-dotdot.deb` | `../../pwned_dotdot` | member skipped by path normalization (`pkg: installed 0 files`), never written |
| `evil-hardlink-alias.deb` | `./d/pwn`, `./d/y -> ../etc`, `./d/h` HARDLINK `./d/y`, `./d/h/autorun` HARDLINK `./d/pwn` | whole package refused (`UnsafePath`, path `/mnt/d/h`); `/mnt/etc/autorun` never created |
| `evil-hardlink-chain.deb` | same primitive with `h2 -> h1 -> y` and a `..`-normalized alias/member path | whole package refused (`UnsafePath`, path `/mnt/d/h1`) |
| `normal-hardlink.deb` | `./usr/bin/hello-real` + `./usr/bin/hello-hard` HARDLINK to it | installs (`pkg: installed 2 files`); one inode with two names, not a copy |
| `normal-hello.deb` | `./usr/bin/hello-pkg`, `./usr/share/normal-hello/readme.txt`, `./usr/bin/hello-alias -> /usr/bin/hello-pkg` | installs (`pkg: installed 3 files`); the absolute symlink is stored verbatim and survives |

`normal-hello.deb` keeps an absolute symlink with no member below it on purpose:
verbatim link targets are a documented installer feature (`alternatives`-style
aliases), so the fix must not reject them — it must only refuse paths that would
be *resolved through* one.

## Running the live proof

```sh
python3 tests/pkg/e2e_pkg_escape.py             # full QEMU run, ~2-4 min on TCG
python3 tests/pkg/e2e_pkg_escape.py --port 8093 # if 8080 is taken
```

The driver generates the packages under `/tmp/pagh-pkg-poc`, starts a local HTTP
server on the port the guest reaches as `10.0.2.2`, then runs
`tools/e2e.py shell --script …` and checks, from the serial log:

* each escape attempt is refused with the offending path named
  (`pkg: install failed: refused unsafe install path '…'`), including the round-2
  hard-link alias (`/mnt/d/h`) and its alias-of-alias chain (`/mnt/d/h1`),
* `cat /mnt/etc/autorun` and `cat /mnt/pwned_root` answer `not found`,
* the `PWN-…` payload markers appear nowhere in the log,
* the plain `..` control installs zero members (skipped, as before),
* the normal packages install their files, the absolute symlink, and a regular
  hard link (two names, one inode),
* a **second boot** of the same data disk (no injection) leaves
  `/mnt/etc/autorun` absent and executes no payload — the persistence half of the
  F-r2-vfs-1 claim.

Exit status: `0` all checks pass, `1` behaviour/guest failure, `2` setup problem
(no QEMU, port busy) — a setup problem is never reported as a security verdict.

## In-kernel / host regressions

* `host-tests/src/properties/tar_links.rs` — F-x86n-1 cases for the pure planner
  (member below an own symlink, later symlink in the archive, relative escaping
  target, hard link path/target, component-boundary false positives, the shipped
  `links-pagh` fixture as a negative control), plus the F-r2-vfs-1 family: the
  round-2 four-member reproducer, hard link to an archive symlink, alias of alias,
  `..`-normalized alias paths, and normal/unknown-target hard links as boundaries.
* `src/selftest_lx.rs::check_install_symlink_confinement` — in-QEMU check of both
  layers: atomic refusal of the attack archive, pre-existing relative in-root link
  still followed, absolute and root-escaping pre-existing targets refused, normal
  archive installed and read back. Runs as part of `python3 tools/e2e.py selftest`.
* `src/selftest_lx.rs::check_install_hardlink_alias_confinement` — the F-r2-vfs-1
  twin: the audit's alias package and an alias-of-alias chain are refused
  atomically, a normal regular-file hard link installs as one inode with two
  names, a single-name pre-existing link is still followed, and a pre-existing
  multiply-linked symlink (an unmarked alias) is not traversed.