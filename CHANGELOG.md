# Changelog

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
