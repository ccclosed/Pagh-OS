# Changelog

Newest first, one paragraph per `Cargo.toml` version.

**How to read this file.** A heading is a version bump, not necessarily a released
build. Git tags / GitHub releases exist for `2.17.0` (2026-11-10), `2.18.6`
(2026-11-10) and the `2.18.12-demo` bundle (2026-11-11, the demo media captured from
2.18.12); `2.17.1`, `2.17.2` and the `2.18.0`-`2.18.12` field-fix series are version
bumps whose notes are recorded here and in `AGENTS.md`, and they roll into the next
tagged release (`2.18.13`, merged as PR #56). The full technical story of every release
is the release-note block in `AGENTS.md`; the paragraphs here name the user-visible
change and point there rather than duplicating it.

## 2.22.5

Round-2 audit finding F-r2-vfs-1 (high, verified): a hard-link alias of a package
symlink still escaped the `.deb` installer's install root. `link(2)` does not
follow the final component, so a tar hard-link member whose target is a symlink
member is a *second name for the symlink inode*, invisible to the 2.22.1
path-based `own_links` check. The audit's package (`./d/pwn`, `./d/y -> ../etc`,
`./d/h` hardlink to `./d/y`, `./d/h/autorun` hardlink to `./d/pwn`) was accepted
and wrote `/mnt/etc/autorun`, which the shell executes as root at the next boot.
The pure planner now computes the hard-link/symlink equivalence closure up front
(aliases of aliases and `..`-normalized targets included, archive order
irrelevant) and refuses the alias member and every member below it, turning the
non-empty refusal list into the same atomic whole-package
`InstallError::UnsafePath` before any write. The effectful layer never creates a
second name for an archive symlink and never traverses a symlink that has more
than one hard link (the backstop for an unmarked alias left by an earlier
install). Normal hard links to regular files and round-1 relative in-root links
(`lib64 -> usr/lib64`) keep working, and the round-1 F-x86n-1 packages are still
refused. Verified with the audit's `attack/r2/vfs/attack-alias.deb` (package
refused as a whole, `/mnt/etc/autorun` absent, a second boot executes no
payload), new `tar_links` host properties, the in-QEMU
`install_hardlink_alias_confinement` selftest and the committed
`tests/pkg/e2e_pkg_escape.py` harness; a normal `.deb` with a regular-file hard
link still installs.

Also fixed, found while producing the in-QEMU evidence: the `lx_selftest`
feature build (not part of CI's six default-profile gates) had been broken since
2.22.2 added `RunError::AccessDenied` — the six `RunError` matches in
`src/selftest_lx.rs` lacked the new arm, so the LXSELFTEST harness could not be
compiled. The arms now report the denial; the default build is unchanged.

Full story: `AGENTS.md`, 2.22.5.

## 2.22.4

Fork-COW frame ownership (round-2 attack audit F-r2-arch-1, critical, and
F-r2-arch-2, high). `fork_user_space_cow` shares leaf frames with `COW_BIT` and
a PMM refcount, but only the exit path honoured it: `munmap`,
`mprotect(PROT_NONE)`, `mprotect(RW)` and `MAP_FIXED` replacement freed or
re-flagged the shared frame without `cow_unref`. An unprivileged process could
`mprotect(RW)` the parent's share, `munmap` it in the forked child, and have the
still-writable parent PTE alias frames the allocator then handed out as kernel
page tables — arbitrary physical read/write, `Creds` zeroed, uid 0. The same
missing accounting freed already-reallocated frames during process exit (1017
PMM double frees). Every user-leaf release now goes through one helper
(`vmm::release_user_leaf`) that frees a frame only when `cow_unref` reports the
last reference and drops the PTE first; `mprotect` re-flags a COW leaf in place,
keeping `COW_BIT` and deferring write access to the next write fault
(`cow_copy_page`); `vmm::map` refuses to overwrite a live COW leaf; the PMM
refuses to free a frame that still has registered COW sharers. The accounting
lives in the pure, host-tested `memory::cow` module (property `cow_ownership`
models fork/write/unmap/MAP_FIXED/mprotect/exit over a PMM that rejects double
frees and frees of mapped frames), and two in-QEMU self-test routines exercise
the real PMM/page tables. Verified with the audit's `cow_root`/`cow_min` PoCs on
the round-2 range: stock 2.22.3 reaches uid 0 and logs the 1017 double frees;
2.22.4 leaves uid 1000, reports no page-table reuse and no `DOUBLE FREE`, and
the shell stays alive.

Full story: `AGENTS.md`, 2.22.4.

## 2.22.3

TOCTOU on user buffers in blocking syscalls (attack-audit F-x86a-4). A sibling
`CLONE_VM` thread could munmap a buffer that `poll`/`ppoll`/`select`/
`epoll_wait` had validated once, after which the kernel touched the stale user
pointer from ring 0 and the page fault halted the machine. The wait loops now
work on kernel copies and re-validate the user range after every yield
(`-EFAULT` when the buffer vanished); the same "validate, block, publish"
pattern was fixed in the read family, the `readv`/`writev` iovec iteration,
`wait4` and `sendfile`. `clone`'s `parent_tid`/`child_tid` writes moved inside
the interrupt-disabled spawn/fork window, and `ioctl(TCSETS*)` now honours its
pointer validation (it used to discard it and read `c_lflag` anyway). Defense
in depth: a kernel-mode page fault on a *user* address kills only the faulting
process; kernel-address and NULL faults stay fatal. Verified with the audit's
PoC (`tools/security/poc/poc_poll_toctou.c`) against the fixed kernel and the
stock 2.22.0 image; the `page_fault` host property covers the fault policy.

Full story: `AGENTS.md`, 2.22.3.
Also fixed in the same batch, found in review: `sendto(2)`/`recvfrom(2)`
validated a capped user range but allocated/copied the raw `u64` count, so a
process with 1 MiB mapped could force a multi-gigabyte kernel allocation
(abort/OOM DoS). Oversized `sendto` is now `EMSGSIZE` before allocation and
`recvfrom` caps the capacity at 1 MiB, with the `p57` host property and
`tools/security/poc/poc_sock_large_count.c` as regressions.

## 2.22.2

The filesystem half of the 2026-11-13 attack-audit batch: crafted ext2 images
and missing path-traversal permission checks. All five x86 VFS findings are
fixed, plus one allocation site of the same class found while fixing them.

* Crafted ext2 symlink inodes no longer drive kernel allocations: the target
  shape is classified from `i_blocks`/`i_size` before anything is allocated —
  fast only when the target is genuinely inline (≤ 60 bytes, `i_blocks == 0`),
  slow only for a one-block data link (≤ 4096 bytes), everything else
  `FsError::Corrupt` (F-x86v-1: `lstat`/`readlink` used to request ~4 GiB and
  abort the kernel).
* Path traversal now requires search (`x`) permission on every intermediate
  directory, checked through the pure link walker's `may_search` hook with the
  process's effective credentials (`access(2)` uses its real ids), including
  directories reached through a followed symlink; denial is `EACCES`, and
  `chdir(2)` additionally needs search on the target directory itself
  (F-x86v-3: uid 1000 could read a 0644 file inside a root-owned 0700
  directory). `lstat`/`readlink` keep no-follow semantics.
* `rename(2)`'s copy emulation is bounded (64 MiB, `ENOMEM` above it) and
  reserves fallibly, so a sparse file claiming ~4 GiB no longer aborts the
  kernel (F-x86v-2).
* `unlink` decides fast/slow for a symlink from `i_blocks` alone, so a crafted
  `i_blocks == 0, i_size > 60` inode can no longer free the block number
  spelled by its target text (F-x86v-4); the read path treats that shape as
  corrupt rather than as a block map.
* Directory block counts are validated (`i_size` block-aligned and not larger
  than the filesystem) before any lookup/readdir/unlink scan, closing the
  ~1M-block crafted-directory stall (F-x86v-5).
* Same class, found in the batch: the `execve`/`PT_INTERP` image reader had an
  unbounded `i_size` allocation and bypassed traversal checks — image size is
  now capped at 256 MiB with a fallible reserve, and the exec path is
  credential-checked (F-x86v-6).

Host properties pin the pure rules (with the audit's exact crafted values) and
the committed ring-3 PoC reproduces every finding on the stock 2.21.0 range
(panics / protected contents / block-count change) while 2.22.2 returns
`ENOENT`/`EACCES`/`ENOMEM`/`ENOEXEC` and stays alive. Remaining audit findings
(the user-buffer TOCTOU and RISC-V runtime items) are listed in
`SECURITY.md` and land in follow-up batches. Full story: `AGENTS.md`, 2.22.2.

## 2.22.1

Package installer confined to its root (F-x86n-1, high, verified). A `.deb` could
carry a symlink member and then a member *below* it: parent resolution followed
the package's own link, interpreted in the guest namespace where `/etc` means
`/mnt/etc`, so `./y -> /etc` + `./y/autorun` wrote `/mnt/etc/autorun` (executed as
root at the next boot) and `./x -> /` + `./x/pwned_root` wrote the install root —
neither member path contains `..`, so the archive-path normalization could not see
it. The pure planner now refuses any member or hard-link target whose ancestor is
a symlink the same archive creates, and the installer rejects the whole package
atomically (`UnsafePath`) before writing anything; the parent resolver walks
lstat-style from the install-root node, never follows a package-created or
absolute-target link, keeps every `..`/relative splice under the root, and
resolves hard-link targets through the same walker, while a pre-existing relative
in-root link (`lib64 -> usr/lib64`) still resolves inside its target. Evidence: the
F-x86n-1 properties in `host-tests/src/properties/tar_links.rs`, the in-QEMU
`install_symlink_confinement` selftest and the committed live harness
`tests/pkg/e2e_pkg_escape.py` (crafted packages refused with a named path,
`/mnt/etc/autorun` absent, plain `..` still skipped, normal `.deb` still installs).
Full story: `AGENTS.md`, 2.22.1.

## 2.22.0

Ring-3 isolation hardening from the 2026-11-13 attack audit. Three independent
ring3-to-ring0 paths and a one-syscall machine kill are closed:

* `mprotect(2)` no longer accepts kernel virtual addresses: `mprotect`/`munmap`/
  `mremap` share a pure user-range gate, the `PROT_NONE` path can only free user
  frames, and `vmm::map` refuses `USER_ACCESSIBLE` mappings of kernel addresses
  (F-LEAD-1: a kernel-heap W+X alias reachable from ring 3, used to zero the
  process's own credentials).
* `rt_sigreturn(2)` rejects frames whose `ip`/`sp` leave the user half and masks
  restored RFLAGS to user-settable bits, so a forged frame can no longer set
  `IOPL=3` and reach I/O ports/PCI config (F-LEAD-2).
* `recvfrom(2)` validates the source-address outputs before writing them (the
  16-byte arbitrary kernel write, F-x86a-1) and `connect(2)` validates the
  caller's `sockaddr` before reading its family (the ring-0 #GP that halted the
  machine, F-x86a-3).
* The EINVAL syscall diagnostic is rate-limited: it is attacker-triggerable and
  left the exploit loop flooding serial and framebuffer.

Verified with the audit's own PoCs against the fixed kernel (exploits now fail
safely; the same PoCs still succeed on the stock 2.21.0 image) and host tests for
the new flag/range rules. Remaining audit findings (package symlink escape,
crafted-ext2 sizes, path search permissions, RISC-V runtime) are listed in
`SECURITY.md` and land in follow-up batches. Full story: `AGENTS.md`, 2.22.0.

## 2.21.0

Console scrollback. The framebuffer console keeps a bounded ring of the rows it
scrolls off (characters and per-cell colour) and shows them again when the view
scrolls: PS/2 mouse wheel (IntelliMouse four-byte packets, negotiated at mouse
init), PageUp/PageDown on the keyboard, three lines per notch. New output keeps
the same absolute rows in view, the status strip reports `scroll N/M`, and an
ordinary key or click returns to the live bottom; `clear` forgets the history. The
view is repainted from the model, so the live grid, caret and selection are
untouched; with a wallpaper active the history view is opaque and the wallpaper is
repainted on return. A USB HID boot mouse has no wheel axis in its protocol, so a
USB mouse scrolls with PageUp/PageDown (stated in `README.md`). The same work fixed
the `wallpaper` shell command never calling `theme::apply`, which made every
wallpaper invisible inside the text grid. Evidence: `drivers::scrollback` host
unit tests and the committed `tools/e2e.py scroll` scenario (QMP wheel, paging,
reset, screenshot diff, wallpaper round-trip). Full story: `AGENTS.md`, 2.21.0.

## 2.20.0

`/proc/<pid>/maps` stops claiming `00:00 0` for files. The map's `PT_LOAD` segments
(image and interpreter) and file-backed `mmap`s now print the `st_dev`/`st_ino` and
pathname `stat(2)`/`fdinfo` already report for the same VFS node — `08:00` is exactly
the `major:minor` this kernel's `stat` returns for a VFS node — plus the real file
offset of a `mmap` (shifted correctly when a partial `munmap`/`mprotect` splits a
region). `[heap]`, `[stack]` and anonymous mappings keep the honest `00:00 0` and no
pathname: they have no backing file. The kernel still keeps no VMA list, so the map
shows exactly what it knows (image, interpreter, heap, tracked `mmap`s, stack) and
nothing invented; DEVIATION-7 is narrowed to those cases. Contract: `docs/procfs.md`
§14. Evidence: the `procfs::maps file identity` in-QEMU selftest,
`properties/procfs_maps.rs` and the `lx_selftest` `maps_metadata` fixture — a real
guest ELF that maps its own file, reads `/proc/self/maps` and writes the dump for the
harness to verify against the VFS.

## 2.19.0

Nine new shell commands plus `ls -l` and `-h` human units for `free`/`df`. `free` prints physical-memory total/used/free (the PMM
frame counters) and the fixed kernel heap (region size, bytes handed out,
remainder) in busybox `free` layout and KiB units; `/proc/meminfo` already gave
programs `MemTotal`/`MemFree`, but the shell had no view and meminfo has no heap
row, and `free` reads the same PMM counters so the two cannot disagree. `date`
prints the CMOS RTC wall clock in UTC (ISO date/time, weekday, Unix seconds) —
`uptime` only ever showed monotonic ticks. `ls -l` prints mode, links,
owner/group, size and symlink targets in long form (no timestamp column: the VFS
has no mtime). `chmod` sets the permission bits `ls -l` prints: ext2 persists
them, filesystems without on-disk metadata report "operation not supported" per
path. `chown` sets the owner/group the same way (numeric ids; ext2 persists,
other filesystems report per path). `tree` prints a recursive ASCII listing with
a counts trailer. `head`, `tail`, `wc` and `grep`
inspect a file without dumping it — first/last N lines,
line/word/byte counts, and the lines matching a pattern — through one streaming
walk with a 4 MiB cap and a transcript-terminating last line. The limits are
stated rather than stubbed: `shared`, `buffers` and `Swap` read zero because this
kernel has no shared-memory accounting, no block cache and no swap device. Full
story: `AGENTS.md`, 2.19.0.

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
the `procfs::per-thread task directories` in-QEMU selftest, real-`clone` `lx_selftest`
checks (thread listing and thread-exit shrink) and `properties/procfs_task.rs`.

## 2.18.13

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
