# Linux userland in pagh (glibc, CPython, apt provisioning)

pagh runs Linux x86_64 ELF binaries in ring 3 through a syscall-compatibility layer
(`lxrun`). As of the current tree this covers **dynamically linked glibc programs**,
including the full CPython 3.13 REPL installed straight from Debian packages.

## What works

- Static `ET_EXEC` and static-PIE binaries (musl/busybox-style).
- Dynamic binaries: the loader reads `PT_INTERP`, maps glibc’s `ld-linux-x86-64.so.2`
  from the ext2 disk, and lets it resolve shared libraries. If the literal interpreter
  path is missing (Debian ships it as a symlink), the loader falls back to the
  merged-`/usr` locations (`/mnt/usr/lib/x86_64-linux-gnu/`, `/mnt/usr/lib64/`,
  `/mnt/lib64/`).
- `LD_LIBRARY_PATH` for `lxrun` children includes `/mnt/usr/lib/x86_64-linux-gnu`,
  where `apt` actually installs libraries.
- The kernel itself is built without SSE/AVX (soft-float), so user-space SIMD
  register state (XMM/MXCSR) is never clobbered by syscalls, interrupts, or
  context switches — the Linux syscall ABI preserves vector registers.
- The `python` shell command finds the installed CPython and runs it with a proper
  `PYTHONHOME`/`PYTHONPATH` environment (`PYTHON_BASIC_REPL=1`).

## First-boot provisioning

A background kernel thread (`src/provision.rs`) makes this work out of the box on a
fresh `disk.img`:

1. Seeds `/mnt` with release metadata, a home skeleton, and a mini-Rust example.
2. Runs `apt update` — streaming the Debian `Packages` index with a gz → xz → plain
   decode fallback and honest `deb:`/`apt:` serial diagnostics on decode failure.
3. Installs `python3` and its dependency closure (≈35 packages, ≈50 MB).

The thread is idempotent; delete `disk.img` to re-provision from scratch.

## Installer details

- tar entries of type symlink/hardlink are **materialized as file copies** (the ext2
  writer has no symlink support). Chains and relative targets are resolved; unresolvable
  links produce one serial warning each.
- Large files are written in bounded WAL transactions (see `EXT2-RECOVERY.md`), so
  multi-hundred-KiB payloads install reliably.

## What does not work yet

These return `-ENOSYS` (logged once per syscall number per process):

- `fork`/`clone`, threads, futexes — single-process programs only.
- Signal delivery. `sigaltstack` is a stub; `tgkill` with a fatal signal aimed
  at the calling process terminates it with the conventional `128+sig` code
  (this is how glibc `abort()` ends).
- `epoll`/`eventfd`/`timerfd` — libuv-based programs (`nvim`) abort at startup.
- Terminal raw mode: `ioctl` `TCGETS`/`TCSETS`/`TIOCGWINSZ` return `EINVAL`, and stdin
  is line-buffered — ncurses TUIs (`htop`) cannot size or raw-mode the terminal.
- No `procfs`/`sysfs`.

So: batch/console programs and the CPython REPL run; event-loop TUIs need the next
stage of the compat layer (signals, epoll, termios, per-keystroke input).

## Troubleshooting

- `lxrun: <path>: file not found or unreadable` for a file that exists usually means
  the **interpreter or a library** was missing, not the binary — check serial for
  `[linux] interpreter … fallback` lines.
- `Package_Installer: … cause=NoSpace` names the file that failed to write.
- Decode problems during `apt update` appear as `deb: inflate …` / `apt: index decode
  failed …` lines before any fallback is tried.
