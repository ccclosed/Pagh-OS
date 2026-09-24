# Linux userland in pagh (glibc, CPython, apt provisioning)

pagh runs Linux x86_64 ELF binaries in ring 3 through a syscall-compatibility layer
(`lxrun`). As of the current tree this covers **dynamically linked glibc programs**,
including the full CPython 3.16 REPL installed straight from Debian packages, and
full-screen TUIs such as `nvim`.

## What works

- Static `ET_EXEC` and static-PIE binaries (musl/busybox-style).
- Dynamic binaries: the loader reads `PT_INTERP`, maps glibc’s `ld-linux-x86-64.so.2`
  from the ext2 disk, and lets it resolve shared libraries. The literal interpreter
  path is resolved **through symbolic links** (`/lib64/ld-linux-x86-64.so.2` is a real
  link now, issue #18), and the merged-`/usr` locations
  (`/mnt/usr/lib/x86_64-linux-gnu/`, `/mnt/usr/lib64/`, `/mnt/lib64/`) remain as a
  fallback for images that ship no link at all.
- `LD_LIBRARY_PATH` for `lxrun` children includes `/mnt/usr/lib/x86_64-linux-gnu`,
  where `apt` actually installs libraries.
- The kernel itself is built without SSE/AVX (soft-float), so user-space SIMD
  register state (XMM/MXCSR) is never clobbered by syscalls, interrupts, or
  context switches — the Linux syscall ABI preserves vector registers.
- Minimal `select`/`pselect6`/`ppoll`: the cooked-tty stdin reports readable and
  the following `read` blocks line-buffered — enough for GNU readline and the
  interactive CPython prompt.
- `mkdir` creates real directories on ext2; tty ioctls on non-tty fds return
  `ENOTTY` and `lseek` on the console returns `ESPIPE`, so a normal CPython
  start no longer floods the serial log with EINVAL diagnostics.
- `mremap` resizes anonymous mappings (shrink in place, grow via move+copy),
  so glibc `realloc()` uses its fast path instead of logging an
  unsupported-syscall warning.
- The `python`/`python3` shell commands find the installed CPython (any `python3*`
  under `/mnt/usr/bin`, preferring the most specific version) and run it with a proper
  `PYTHONHOME`/`PYTHONPATH` environment (`PYTHON_BASIC_REPL=1`).
- `fork`/`clone`: `clone` handles both the thread and fork paths; a fork creates a real
  child with a copied address space (`fork_linux_process`). Threads via
  `CLONE_VM|CLONE_THREAD` share the address space; glibc `pthread_create` works.
- `epoll` (create1/ctl/wait/pwait) and `eventfd2` are implemented over the compat fd
  table, plus unix sockets (`socketpair`/AF_UNIX stream), so libuv event loops run —
  `nvim` starts, renders through the VT emulator, and saves its ShaDa
  (`rename`/`renameat`/`renameat2`, `fsync`/`fdatasync`, `readv`/`preadv`/`pwritev`).
- Terminal: the console is a real tty. Honest `tcgetattr` reports actual
  `ICANON`/`ECHO` state, `TCSETS*` applies raw mode per termios, and in raw mode the
  kernel echoes input itself; `ioctl(FIONBIO)` toggles non-blocking fds (libuv
  `uv__nonblock`); `TIOCGWINSZ`/`TIOCSWINSZ` work. The VT driver emulates CSI/ECMA-48
  sequences (including cursor interrogation replies, charset designation `ESC ( B`,
  and `ONLCR`).
- Job control probes: `setpgid`/`getpgid`/`getpgrp` stubs (single process group),
  `TIOCGPGRP`/`TIOCSPGRP`, and `wait4` accepting `WUNTRACED`/`WCONTINUED` — enough for
  bash's job-control hot loop not to spin.

## First-boot provisioning

A background kernel thread (`src/provision.rs`) makes this work out of the box on a
fresh `disk.img` — **after an opt-in confirmation**:

1. Seeds `/mnt` with release metadata, a home skeleton, and a mini-Rust example.
2. Asks on the console: `download & install glibc + python3 in the background? [Y/n]`.
   `N` skips everything; `apt update` + `apt install python3` from the shell installs
   the userland later at any time.
3. On `Y`, runs `apt update` — streaming the Debian `Packages` index with a gz → xz →
   plain decode fallback and honest `deb:`/`apt:` serial diagnostics on decode failure.
4. Installs `python3` and its dependency closure (≈35 packages, ≈50 MB).

The thread is idempotent; delete `disk.img` to re-provision from scratch.

## Installer details

- The ext2 writer and the VFS resolve real symlinks and hard links: `readlink`, `lstat`
  (S_IFLNK), `stat`/`open` following, `ELOOP` on cycles, `st_nlink` from `i_links_count`
  (issue #18). The tar side (t16) still materializes symlink/hardlink members as file
  copies until it is switched over; see `EXT2-LINKS.md`.
- Large files are written in bounded WAL transactions (see `EXT2-RECOVERY.md`), so
  multi-hundred-KiB payloads install reliably.

## What does not work yet

These return `-ENOSYS` or a stub (logged once per syscall number per process):

- Signals: delivery is real but happens **only at the syscall-return point**
  (`rt_sigaction` handlers get a real `rt_sigframe`, `rt_sigreturn` restores the
  context, `EINTR` wakes the patched blocking waits, `kill(2)`/`tgkill` send
  signals including process-group broadcasts, and `SIGSTOP`/`SIGCONT` park/resume
  the task with `wait4(WUNTRACED)`/`WCONTINUED` reporting both state changes).
  Delivery from the timer-tick return path is still missing, so a CPU-bound loop
  that never enters a syscall does not see a signal.
- `timerfd` — programs needing timerfd descriptors still fail.
- `procfs` covers the first slice only (issue #11): `/proc/cpuinfo`, `/proc/meminfo`,
  `/proc/uptime` and `/proc/self/{exe,cmdline,status,maps}`, rendered from live kernel
  state (contract: `docs/procfs.md`). `/proc/<pid>`, `/proc/stat`, `/proc/loadavg` and
  `/proc/self/fd` return `ENOENT`, so `htop`/`ps` still do not work; `sysfs` does not
  exist at all.
- Symbolic/hard links exist on ext2 and resolve through the VFS (issue #18); the *installer*
  still materializes tar links as copies until t16 switches it over (`EXT2-LINKS.md`).

So: batch/console programs, the CPython REPL, and event-loop TUIs (`nvim`) run; the
remaining gaps are timer-tick signal delivery/SIGSTOP-SIGCONT, timerfd, and procfs.

## Troubleshooting

- `lxrun: <path>: file not found or unreadable` for a file that exists usually means
  the **interpreter or a library** was missing, not the binary — check serial for
  `[linux] interpreter … fallback` lines.
- `Package_Installer: … cause=NoSpace` names the file that failed to write.
- Decode problems during `apt update` appear as `deb: inflate …` / `apt: index decode
  failed …` lines before any fallback is tried.
