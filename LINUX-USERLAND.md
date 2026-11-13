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
- Job control: real process groups (`pgid` in `CompatState`, `setpgid`/`getpgid`/
  `getpgrp`/`setsid`, `kill(0)`/`kill(-pgid)` addressing the group), a kernel-side
  foreground process group on the console terminal (`TIOCGPGRP`/`TIOCSPGRP`),
  `SIGTTIN`/`SIGTTOU` on background tty I/O, `wait4` with `WUNTRACED`/`WCONTINUED`,
  `SIGCHLD` queued on child exit/stop/continue, and `^Z` stopping the foreground
  *group* (`SIGTSTP`); the shell keeps a job table (`jobs`/`fg`/`bg`).

## First-boot provisioning

`src/provision.rs::seed()` runs once per freshly formatted disk, straight from the boot
path after `/mnt` is mounted, and is idempotent (its guard is `/mnt/etc/pagh-release`):

1. Seeds `/mnt` with release metadata (`etc/pagh-release`) and a motd that carry the same
   crate version, a home skeleton (`home/user/`), `/usr/share/pagh/`, a `resolv.conf`
   pointing at slirp's DNS (`10.0.2.3`), and a mini-Rust example (`examples/hello`).
2. Installs nothing over the network and asks nothing on the console: boot no longer
   stops at a Y/n question, because a console prompt without a timeout is not something
   boot may do.

The glibc + CPython userland is installed on demand from the shell instead:
`apt update` streams the Debian `Packages` index (gz/xz/zstd decode, honest `deb:`/`apt:`
diagnostics on failure), then `apt install python3` pulls its dependency closure
(≈35 packages, ≈50 MB). Delete `disk.img` to re-seed from scratch.

## Installer details

- The ext2 writer and the VFS resolve real symlinks and hard links: `readlink`, `lstat`
  (S_IFLNK), `stat`/`open` following, `ELOOP` on cycles, `st_nlink` from `i_links_count`
  (issue #18), and the installer creates tar symlink/hardlink members as real links rather
  than copies; see `EXT2-LINKS.md`.
- Large files are written in bounded WAL transactions (see `EXT2-RECOVERY.md`), so
  multi-hundred-KiB payloads install reliably.

## What does not work yet

Most of the Linux surface this document used to list is now implemented. The gaps that
remain are named here rather than implied:

Signals and the shell job table are implemented (syscall-return and timer-tick
delivery, real `rt_sigframe`/`rt_sigreturn`, `kill(2)`/`tgkill` with process groups,
`SIGSTOP`/`SIGCONT`, `SIGCHLD` on exit/stop/continue, `^Z` on the foreground group);
`timerfd`/`signalfd`/`memfd_create` exist, and procfs covers slices 1–6 (issue #11):
`/proc/{cpuinfo,meminfo,uptime,stat,version,filesystems,loadavg,mounts,self/*,<pid>/*}`,
`/proc/<pid>/{fd,fdinfo,limits,cgroup,mountinfo}`, `/proc/<pid>/maps` with the
real `st_dev`/`st_ino`/offset/pathname of file-backed regions, the per-thread view
`/proc/<pid>/task/` + `/proc/self/task/` (one directory per live task of the thread
group, `task/<tid>` is the same VFS object as `/proc/<tid>`, so `stat`/`status`/
`cmdline` are per-thread) and read-only
`/proc/sys/kernel/{ostype,osrelease,hostname}`, all rendered from live kernel state
(contract: `docs/procfs.md`; `/proc/loadavg` carries real decayed 1/5/15-minute
averages). What this document still lists as missing, per that contract:

- `sysfs` does not exist at all.
- procfs deferrals: mountinfo optional fields, statfs metadata and the Linux
  per-thread files the kernel does not model (`sched`, `syscall`, per-thread `comm`).
- tty/job-control deviations (stated in `AGENTS.md` and `drivers::tty` rather than
  implied): no sessions (`getsid`/the `tcsetpgrp` session rule are approximated), a
  background tty access returns `EINTR` instead of being restarted after `SIGCONT`,
  `TOSTOP` defaults **on** where Linux defaults it off, `tcsetattr` from a background
  group does not raise `SIGTTOU`, and `kill(-pgid)` queues one copy on the group leader
  rather than one per thread group (the terminal/shell stop/continue broadcasts do use
  per-thread-group delivery).
- The `pkg <host> <path>` (index-free) downloader stays unverified by construction;
  `apt` is the verified path (`SECURITY.md`).

So: batch/console programs, the CPython REPL, and event-loop TUIs (`nvim`) run; the
gaps left are the ones named above, plus the deliberately unverified manual `pkg` path.

## Troubleshooting

- `lxrun: <path>: file not found or unreadable` for a file that exists usually means
  the **interpreter or a library** was missing, not the binary — check serial for
  `[linux] interpreter … fallback` lines.
- `Package_Installer: … cause=NoSpace` names the file that failed to write.
- Decode problems during `apt update` appear as `deb: inflate …` / `apt: index decode
  failed …` lines before any fallback is tried.
