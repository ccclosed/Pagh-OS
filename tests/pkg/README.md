# `tests/pkg/` — package-installer security fixtures

Regression material for **F-x86n-1** (2026-11-13 attack audit, high/verified): a
`.deb` could carry a symlink member that redirects later archive members outside
the installation root, ending in `/mnt/etc/autorun` — executed as root at the next
boot — or at the install root itself (`/mnt/pwned_root`). The fix lives in
`src/pkg/install.rs` (pure planner) and `src/pkg/install_fs.rs` (confined
resolver); its contract is described in `SECURITY.md`, `AGENTS.md` 2.22.1 and
`EXT2-LINKS.md` §5.6.

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
  (`pkg: install failed: refused unsafe install path '…'`),
* `cat /mnt/etc/autorun` and `cat /mnt/pwned_root` answer `not found`,
* the `PWN-…` payload markers appear nowhere in the log,
* the plain `..` control installs zero members (skipped, as before),
* the normal package installs its files and its absolute symlink.

Exit status: `0` all checks pass, `1` behaviour/guest failure, `2` setup problem
(no QEMU, port busy) — a setup problem is never reported as a security verdict.

## In-kernel / host regressions

* `host-tests/src/properties/tar_links.rs` — F-x86n-1 cases for the pure planner
  (member below an own symlink, later symlink in the archive, relative escaping
  target, hard link path/target, component-boundary false positives, the shipped
  `links-pagh` fixture as a negative control).
* `src/selftest_lx.rs::check_install_symlink_confinement` — in-QEMU check of both
  layers: atomic refusal of the attack archive, pre-existing relative in-root link
  still followed, absolute and root-escaping pre-existing targets refused, normal
  archive installed and read back. Runs as part of `python3 tools/e2e.py selftest`.