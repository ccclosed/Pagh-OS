# Changelog

## 2.16.1 — 2026-11-09

Fixes found by running real apt-installed binaries (fastfetch) and a persistent
disk:

- `mprotect` no longer returns ENOMEM for a range whose pages are resident but
  not tracked as mmap/brk regions (the ELF loader maps PT_LOAD segments without
  a VmRegionSet entry). glibc's ld.so reported this as "cannot apply additional
  memory protection after relocation"; fastfetch now starts.
- COW write faults are serviced for every `COW_BIT` page, not only pages inside
  a tracked writable VMA or the brk heap: the user stack, thread stacks and
  fork-shared heap pages are real writable mappings, and a bookkeeping miss
  killed both parent and child immediately after `fork` in fastfetch.
- The boot fs demo truncates `bootdemo.txt` before rewriting it, so a shorter
  demo string on a persistent disk no longer leaves the previous run's tail and
  reports a false read-back MISMATCH.

Verified live: `lxrun /mnt/usr/bin/fastfetch` prints Kernel/Uptime/Terminal/
CPU/Memory/Disk and exits cleanly; the fs demo is PASS again; 414 host tests and
the 82/83-routine e2e selftest remain green.

## 2.16.0 — 2026-11-09

Batch release: Linux compat, procfs slice 5, job control and the NVMe rewrite.

### Linux compat
- `signalfd`/`signalfd4`: readable fd over pending signals, `POLLIN` in poll/select/epoll.
- Real process groups: `pgid` in `CompatState`, `setpgid`/`getpgid`/`getpgrp`/`setsid`, group `kill`.
- `memfd_create`, `timerfd_create/settime/gettime` (read/poll/epoll integrated).
- Broad syscall batches: `*at` family, `sendfile`, `copy_file_range`, legacy/advisory
  calls, `setrlimit`/`getrusage`/`times`.

### Job control
- Ctrl+Z stops the foreground process group (SIGTSTP); shell job table with
  `jobs`/`fg`/`bg` and SIGCONT resume.
- Kernel controlling-terminal foreground pgid, `TIOCGPGRP`/`TIOCSPGRP`,
  SIGTTIN/SIGTTOU on background tty I/O; `lxrun` hands the terminal to the job
  and restores it.

### procfs slice 5
- `/proc/<pid>/fdinfo/<n>` (real `pos`/`flags`/`mnt_id`/`ino`) and
  `/proc/<pid>/limits` (16 Linux rlimit rows), plus `self` variants.

### NVMe
- Multi-page PRP lists, up to 64 KiB per command, four commands in flight,
  completion reaping by CID, and the bring-up/protocol fixes that made QEMU NVMe
  usable (Identify NN offset, Create-IOCQ/IOSQ fields, Write/Flush opcodes).

Gates: 414 host tests, e2e selftest 83 routines / 0 failed, all AGENTS gates green.

## 2.15.0 — 2026-11-08

Batch release: 15 commits, focused on the Linux compatibility layer and two GUI
fixes.

### Linux compat
- `faccessat`/`fchmodat`, `mkdirat`/`unlinkat`/`fchownat` — modern `*at` family
  (AT_FDCWD-equivalent dirfd model; `AT_EACCESS`/`AT_SYMLINK_NOFOLLOW`).
- `sendfile` (regular-file input to file/console/pipe/socket) and
  `copy_file_range` between regular files.
- Legacy/auxiliary numbers: `eventfd`, `epoll_create`, `getcpu`,
  `sched_getaffinity`/`sched_setaffinity`, `fadvise64`.
- `setrlimit`, `getrusage` (honest zeroed fields), `times`.
- Broad advisory/runtime batch: `faccessat2`, `readahead`, `sync`, `syncfs`,
  `getpriority`, `setpriority`, `personality`, `sched_getparam`,
  `sched_setscheduler`, `sched_getscheduler`, `sched_get_priority_{max,min}`,
  `sched_rr_get_interval`, `rt_sigpending`, `sync_file_range`, `fallocate`
  (mode 0).
- `memfd_create`: anonymous RAM-backed fd (`MFD_CLOEXEC`,
  `MFD_ALLOW_SEALING` accepted; no seal syscalls).
- `timerfd_create`/`settime`/`gettime`, integrated with read/poll/select/epoll.
- SIGCHLD is queued to the parent on child exit/stop/continue (job-control
  process groups and `^Z` tty stops remain open).

### procfs / shell
- `/proc/self/mountinfo` and `/proc/<pid>/mountinfo` with Linux's field order.
- Shell `df` (real ext2 capacity, `-` for synthetic filesystems) and `ps`
  (PID/PPID/STAT/COMMAND from `/proc/<pid>`).
- `/proc/self/cgroup` + `/proc/<pid>/cgroup` (`0::/`, no controllers).

### GUI
- Mouse selection repaints incrementally: a large drag no longer repaints the
  whole range on every mouse event (fixes the FPS drop while selecting text).

### Tooling
- Removed the unused `pagh_mcp.py` helper.
- Release cadence is 15 commits per tag; version = highest-impact change.
