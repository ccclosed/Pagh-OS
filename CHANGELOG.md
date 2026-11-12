# Changelog

Newest first, one paragraph per `Cargo.toml` version.

**How to read this file.** A heading is a version bump, not necessarily a released
build. Git tags / GitHub releases exist for `2.17.0` (2026-11-10), `2.18.6`
(2026-11-10) and the `2.18.12-demo` bundle (2026-11-11, the demo media captured from
2.18.12); `2.17.1`, `2.17.2` and the `2.18.0`-`2.18.12` field-fix series are version
bumps whose notes are recorded here and in `AGENTS.md`, and they roll into the next
tagged release (`2.18.13`, open in PR #56). The full technical story of every release
is the release-note block in `AGENTS.md`; the paragraphs here name the user-visible
change and point there rather than duplicating it.

## 2.19.0 (in this PR, not on `main` yet)

The per-thread procfs view. `/proc/<pid>/task/` and `/proc/self/task/` now list one
numeric directory per **live task of the process's thread group** — pagh has real
threads (`clone(CLONE_THREAD)` registers a scheduled task under its own pid with a
shared `tgid`), but until now they could only be seen as top-level `/proc/<tid>`
entries. Each thread's directory is deliberately the **same VFS object as
`/proc/<tid>`** (Linux identity: one task, one inode), so `stat`, `status`, `cmdline`
and the rest of the pid-directory files are the per-thread view with Linux's 52-field
`stat` line and `Tgid`/`Pid`/`Threads` in `status`. A dead pid, a tid from another
thread group and a non-numeric component are `ENOENT` — the directory is a namespace,
not a wildcard. `/proc/self/task` follows the reader's own group and is `ENOENT` for a
task without `CompatState`, like the rest of `/proc/self`. Contract (paths, semantics,
inode identity and the deviations the model keeps) in `docs/procfs.md` §13; evidence is
the `procfs::per-thread task directories` in-QEMU selftest, a real-`clone` `lx_selftest`
check and `properties/procfs_task.rs`.

## 2.18.13 (in PR #56, not on `main` yet)

xHCI diagnostics: the Supported Protocol PORT INFO word is now parsed as the spec
defines it (PSIC in bits 31:28, count in 15:8, 1-based offset in 7:0). The old parser
read the count from 31:16 and the offset from 15:8 + 1, which is why every real USB2
port printed `proto=usb0` in the 2.18.12 field photo. Diagnostics only: the port
reset/adopt policy is unchanged. Full story: `AGENTS.md`, 2.18.13.

## 2.18.12

The halted-controller release. The 2.18.11 field photos showed `usbcmd=0`,
`usbsts=0x15`: the xHCI was **halted** at shell time and every USB2 port sat in Polling
with `try=19`. Three real bugs followed: the DCBAA and the scratchpad-array frame were
`start_controller` locals that `DmaFrame::drop` returned to the PMM while the
controller still owned them; xHCI was the only DMA driver here that never called
`pci::enable_bus_master`; and every USB wait was a spin count, i.e. a different
wall-clock duration on every machine, so the port budgets were a few milliseconds of
real time. The driver now owns those frames, enables bus mastering before touching the
BAR, calibrates the TSC against a PIT channel-2 one-shot and uses real deadlines, and
`poll()` detects a halted controller and restarts it. Smaller fixes: wait out a
mid-detect port instead of resetting it mid-attach, resume an enabled-but-suspended one
(PLS=U3), issue WPR only on a SuperSpeed link, and print the protocol port masks plus
the raw capability entries. Full story: `AGENTS.md`, 2.18.12.

## 2.18.11

The boot menu that stopped counting: `xhci::poll()` processed every port in one call, and
on hardware a single reset attempt costs hundreds of milliseconds of MMIO waits, so the
first poll blocked for seconds inside the menu's one-second slice and the 20 s countdown
never ticked. `poll()` now initialises exactly one port per call, round-robin, the wait
budgets were cut to a fraction of a second, and the diagnostic prints the USB2/USB3 port
ranges. Full story: `AGENTS.md`, 2.18.11.

## 2.18.10

The extended capabilities were never reached: the walk gave up at 4 KiB while the field
machine reports HCCPARAMS1 xECP = 32 KiB, so the BIOS/USB-legacy handoff had never run
(`legacy=absent`) and the Supported Protocol entries had never been seen (`proto=usb0`
everywhere). The walk limit is now the mapped 64 KiB. A port the controller has already
enabled is adopted instead of being reset back into Polling, and the PP power-cycle
fallback runs every fourth attempt instead of every attempt (it was what made the mouse
LED blink). Full story: `AGENTS.md`, 2.18.10.

## 2.18.9

Retries that outlive the settle: a failed enumeration used to leave its xHCI slot
enabled, and the poll loop skips any port that has a slot, so the ports that got furthest
could never be retried. The driver now sends Disable Slot, clears the DCBAA entry and
drops the slot's rings (freeing their DMA pages through a `Drop` impl), and ports are
retried forever with exponential backoff instead of a bounded count, because a PCH USB2
PHY settling after `HCRST` can take seconds. Full story: `AGENTS.md`, 2.18.9.

## 2.18.8

The brief enable, and the ports the reset costs: the controller enables the device and
then loses it again on every attempt (`ped_sc` is a healthy high-speed enable,
`post=0x6e1` is back in Polling). Two blind spots were fixed: the Supported Protocol
capabilities were never read (so every port said `proto=usb0` and a USB2 keyboard port
could not be told from a USB3 one), and the extended-capability walk now falls back to
the 0x20 area when HCCPARAMS1 reports xECP=0. Diagnostics gained HCIVERSION /
HCSPARAMS1 / HCCPARAMS1 and the attached-before-reset vs now lists; ports with no
device are retried forever at a slow rate. Full story: `AGENTS.md`, 2.18.8.

## 2.18.7

The port reset that does not stick: a single PED sighting was being accepted as success,
and the port then fell back to Polling. The driver now requires a STABLE enabled state
(PED with link state U0) before addressing the device; a failed stability check falls
through to a warm reset and then to a PP power-cycle with real settling delays, and the
diagnostics gained the port's USB protocol generation plus the PORTSC snapshot at the
first PED sighting. Full story: `AGENTS.md`, 2.18.7.

## 2.18.6 — released 2026-11-10

The 2.18.5 photo shows the port reset never taking effect and QEMU cannot distinguish
"the controller ignores the writes" from "the driver reads the wrong register", so this
release puts the ambiguity on screen: the controller scan (both PCI controllers, their
port counts and attached masks, which one is in use), a raw dump of the selected port's
four register-set dwords, USBCMD/USBSTS/CONFIG and CAPLENGTH, and a write-read-back test
of a harmless register that says whether ANY write this driver issues reaches the
controller. Full story: `AGENTS.md`, 2.18.6.

## 2.18.5

The port-reset signal. The 2.18.4 per-port diagnostic came back with
`stage=address code=0x4` and a settled `ccs=1 ped=0 pls=7`: devices present, ports never
ENABLED, the address request sent to a device that had never left Polling. The driver had
been treating port-reset-change (PRC) as "reset done", but on real hardware a reset the
device does not complete still raises PRC while PED stays 0. Port bring-up now waits for
PED and only then reads the speed and addresses the device; a hot reset that does not
enable the port retries with a warm reset, then with a PP power-cycle, and the failure
row carries the before/after PORTSC snapshots. Full story: `AGENTS.md`, 2.18.5.

## 2.18.4

The field test answered the 2.18.3 counter with its own numbers ("2 controllers, 26
ports, devices on six ports") and **0 HID**: the controller saw the keyboard and
enumeration stopped somewhere the old build could not name. Boot now prints one row per
port with the enumeration stage it reached, the shell banner carries the last failure,
and the `usb` command prints the same plus every PORTSC. Three real-hardware fixes: EP0's
max packet size is updated from the device descriptor (full-speed devices start at 8 and
the 18-byte read fails otherwise), USB3 ports whose link firmware left suspended get a
warm reset, and a port whose enumeration fails is retried with growing spacing. Full
story: `AGENTS.md`, 2.18.4.

## 2.18.3

The first field report after the disk menu: the clock still ran slow and the USB
keyboard/mouse were dead on the real machine. The clock had one cause — the LAPIC
calibration call site had been left disabled during the BIOS-stack debugging. For input,
the driver now finds the keyboard whichever xHCI controller it is plugged into (probe
every controller, count attached ports, start the best), takes ownership from the
firmware's USB legacy support, and RETRIES ports that have not produced a device yet
instead of giving up on the first look, because USB2 connect needs ~100 ms of debounce
that QEMU's instant answer had hidden. A device behind a hub is detected and said out
loud (hubs are still not supported), and the boot menu and shell banner print one line —
controllers, ports, HID devices, reports — so a no-serial failure is visible on screen.
Full story: `AGENTS.md`, 2.18.3.

## 2.18.2

USB input that actually works in QEMU: the enumeration path had never run, because four
defects stacked — xHCI port registers were addressed with a 4-byte stride instead of 16
(so "port 5" was really port 2's PORTSC), Address Device was handed the HDM-virtual
address of the input context instead of its physical one, SET_CONFIGURATION was never
sent (leaving the HID endpoint stalled), and the endpoint context put CErr/MaxPacket in
the wrong dword with a raw `bInterval`, so the interrupt-IN endpoint was configured as
ISO OUT with a zero packet size. With those fixed an emulated USB keyboard enumerates,
reports, and types into both the shell and the boot menu, including arrows/Enter/Esc;
the report poll no longer spins 50 000 iterations per empty poll. Full story:
`AGENTS.md`, 2.18.2.

## 2.18.1

The disk menu and the ownership rule: boot shows a menu of the partitions it can see
(drawn straight into the framebuffer before the shell exists) with what each holds —
pagh's own volume, an empty region, a `FOREIGN` filesystem, or unknown data. `Enter`
mounts, `F` formats only after the device name is typed back, `Esc` skips, and a 20 s
timeout takes the non-destructive choice. A volume is pagh's own iff it carries pagh's
WAL superblock (`Ext2Fs::is_pagh_volume`), so ext3/ext4 are refused by their feature
words, a foreign ext2 is left untouched, and the only device the automatic path may
format is the RAM disk from the boot medium. Three bugs surfaced while building it: the
ext-family superblock magic was probed at 1024 instead of 1024+0x38 (every real ext2/ext4
volume classified as `Foreign`), the boot path now runs on its own 128 KiB stack because
Limine's is too small on BIOS boots, and the boot menu is ASCII (the kernel font covers
no Cyrillic). Full story: `AGENTS.md`, 2.18.1.

## 2.18.0

The first fixes driven by a real machine reaching the shell: the LAPIC timer is
**measured** against the PIT at boot instead of assuming QEMU's 1 GHz bus clock (on
hardware the assumed constant made every tick ~60x too long, so the shell's clock
advanced one second per minute and every timeout stretched the same way);
`drivers::partition` reads GPT (and MBR) and publishes each partition as its own block
device, so pagh can own one partition instead of claiming the whole disk; and `logfile`
mirrors every kernel log line into `/mnt/pagh-log.txt` — buffered in a fixed ring from
the first boot message (the logger runs in interrupt and panic contexts, where touching
a filesystem would deadlock) and drained by a `logd` thread once `/mnt` is mounted — so a
machine with no serial port leaves a readable log on its disk. Full story: `AGENTS.md`,
2.18.0.

## 2.17.2

The real cause of the 2.17.1 field failure. The `#GP` was in `vmm::map`, and the LAPIC
addresses firmware reported were correct: the page-table walker treated a **huge page**
as a page table. Limine maps the HHDM with 2 MiB (sometimes 1 GiB) pages, so a walk into
the LAPIC's HHDM window found a present entry whose HUGE_PAGE bit was set and whose
"frame address" is the start of a large RAM region, not 512 entries; indexing that
produced a non-canonical pointer, which is a general-protection fault rather than a page
fault. `ensure_next` now takes the entry's level and splits a huge entry into a real table
(2 MiB -> 512 x 4 KiB, 1 GiB -> 512 x 2 MiB, flags preserved), and `map_mmio` refuses
implausible physical bases. QEMU had booted only because its HHDM happened to have no
huge page at that address. Full story: `AGENTS.md`, 2.17.2.

## 2.17.1

The first field report from real hardware. The emergency console named the step
(`init_apic`) and caught a `#GP` inside the LAPIC page-table walk — a general-protection
fault rather than a page fault, which means the virtual address computed from the MADT
base was non-canonical, i.e. the firmware reported a garbage LAPIC address. `vmm::map_mmio`
now refuses an implausible physical base with a message instead of faulting inside the
walker, `apic::init` prints the addresses it was given and falls back to the architectural
`0xFEE00000` instead of panicking, x2APIC-enabled handover is detected and switched back
to xAPIC, and boot announces each step *before* running it so the step that hangs names
itself. Full story: `AGENTS.md`, 2.17.1.

## 2.17.0 — released 2026-11-10

The batch that made pagh run on real hardware, plus three long-standing display/network
defects: a peer that answers and closes in one burst now completes `connect(2)` — the
completion test is `ever_established` (the handshake happened), not
`state == Established`, a window a reply+FIN skips straight through, which is what made
the clipboard bridge work once and then fail; the VT emulator and the text console share
one coordinate system and hand the cursor over in both directions inside the spawn's
`without_interrupts` window, so `lxrun` no longer clears the screen black or marches the
prompt downwards; and `ps` lists kernel threads (`[shell]`, `[net]`, `[usb]`, ...) while
`df` reports the bytes a ramfs is holding. Boot now shows a disk menu
(`src/boot_menu.rs`), USB input goes through `drivers::xhci` + `drivers::usb_hid`
(controller bring-up and report translation verified; device enumeration was not yet),
`drivers::earlyfb` gives a machine with no serial port a visible boot trace, and
`tools/mkimage.py` builds both artifacts people actually boot — a GPT+ESP USB image and
a BIOS+UEFI ISO — each carrying `disk.img`. Full story: `AGENTS.md`, "2.17.0".

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
