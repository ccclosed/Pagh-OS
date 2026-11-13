# Security regression PoCs (2026-11-13 attack audit)

Committed copies of the exploit sources from the red-team report, so the fixes
they motivated can be re-run later. The boot range itself (a frozen ESP, base
disk, `cc-static`, `run.sh`) lives **outside this repository**; point `RANGE` at
it. Nothing here builds into the kernel or runs in CI.

```sh
RANGE=/home/oleg/Projects/pagh-os/attack/range   # private range from the report
```

## What each PoC proves

| File | Finding | Vulnerable 2.21.0 | Fixed (2.22.0-2.22.3) |
|---|---|---|---|
| `kheap.c` | F-LEAD-1: `mprotect()` on kernel VAs → ring-3 W+X alias of the kernel heap → zero the process's `Creds` → uid 0 | `PWN-KHEAP: SUCCESS root file contents: ROOT-ONLY-SECRET-7f3a` | `PWN-KHEAP: mprotected pages=0x0` + `creds pattern not found` (uid stays 1000) |
| `iopl.c` | F-LEAD-2: forged `rt_sigreturn` frame sets `RFLAGS.IOPL=3` → ring 3 I/O ports / PCI config | `PWN-IOPL: RFLAGS=0x…3202` then `PWN-IO: port 0x80`, `PWN-IO: pci dev=3 id=0x10011af4` | `PWN-IOPL: RFLAGS=0x…0202`, dies on the first port access; no `PWN-IO:` lines |
| `poc_connect_gp.c` | F-x86a-3: `connect()` reads `sa_family` through an unvalidated pointer → ring-0 #GP → machine halt | no shell prompt after `[autorun]` (machine halted) | `POC-CONNECT-GP: SURVIVED r=-1 errno=14` (EFAULT), shell prompt returns |
| `poc_recvfrom_write.c` | F-x86a-1: `recvfrom()` writes `sockaddr`/`socklen_t` through unvalidated pointers (16-byte arbitrary kernel write) | `PWN-RECVFROM: arbitrary kernel write confirmed: MONOTONIC jumped …` | the validated write path refuses, so no datagram is reported as received; `PWN-RECVFROM` never prints |
| `poc_poll_toctou.c` | F-x86a-4: blocking syscalls (`poll`/`ppoll`/`select`/`epoll_wait`, plus `clone` TID outputs) use a user buffer after a `yield`; a CLONE_VM sibling munmaps it → kernel-mode #PF → machine halt. Modes: `poll`, `ppoll`, `select`, `epoll`, `futex` | `poll` mode: no shell prompt after `[autorun]`; serial ends with `[EXC #14] PAGE FAULT … U=false` at the pollfd page (kernel-mode fault, machine halted) | `POC-TOCTOU[poll]: SURVIVED poll r=-1 errno=14` (EFAULT) and the shell prompt returns; same EFAULT for `ppoll`/`select`/`epoll_wait`/`futex` |

## Reproduction (same host, two kernels)

```sh
# 1. fixed kernel: build, then run the PoCs against a private copy of the range
python3 tools/build.py build --release
rm -rf /tmp/range-fixed && cp -a "$RANGE" /tmp/range-fixed
cp target/x86_64-unknown-none/release/pagh.elf /tmp/range-fixed/esp/pagh.elf

# dynamic glibc PoCs (kheap, iopl) — host gcc, as in the report
cc -O2 -no-pie -o /tmp/kheap tools/security/poc/kheap.c
cc -O2 -no-pie -o /tmp/iopl  tools/security/poc/iopl.c
cd /tmp/range-fixed
./run.sh --expect 'PWN-KHEAP: creds pattern not found' /tmp/kheap
./run.sh --expect 'PWN-IOPL: RFLAGS=0x0000000000000202' /tmp/iopl

# The F-x86a-4 TOCTOU PoC (2.22.1): one static ELF, one mode per syscall. The
# sibling is raw assembly because a libc clone wrapper returns the child
# through the parent's call frame (see the source header).
"$RANGE/cc-static" -O2 -o /tmp/poc_poll_toctou tools/security/poc/poc_poll_toctou.c
for mode in poll ppoll select epoll futex; do
    ./run.sh --expect "POC-TOCTOU\[$mode\]: SURVIVED" /tmp/poc_poll_toctou "$mode"
done

# static PoCs, built against the symbols of the kernel under test
TICK=$(nm esp/pagh.elf | awk '$3 ~ /TICK_COUNT$/ {print $1; exit}')
"$RANGE/cc-static" -O2 -o /tmp/poc_connect tools/security/poc/poc_connect_gp.c
"$RANGE/cc-static" -O2 -DTICK_COUNT_ADDR=0x$TICK \
    -o /tmp/poc_recvfrom tools/security/poc/poc_recvfrom_write.c
./run.sh --expect 'POC-CONNECT-GP: SURVIVED' /tmp/poc_connect
./run.sh --net /tmp/poc_recvfrom        # default success: shell prompt after autorun
```

The vulnerable side is the same commands with the **unmodified** range (no
`.esp/pagh.elf` swap); the table above is the recorded contrast.

## Not covered yet

These five findings were the 2.22.0-2.22.3 batches. The audit's remaining items
(RISC-V userland runtime) are listed in `SECURITY.md`; the x86 findings now have
committed PoCs here (the F-x86a-4 TOCTOU PoC is `poc_poll_toctou.c`, above).

## VFS/ext2 regressions (2.22.2) — the crafted-ext2 / `walk()` items above

The x86 VFS/ext2 batch from the same audit (crafted ext2 size fields, missing
path-traversal permission checks, `rename`'s sparse copy, fast/slow symlink
confusion) now has committed PoCs too. `poc/x86vfs_poc.c` is a raw-syscall ELF
that drops to uid 1000 and exercises one selector per finding;
`poc/x86vfs_craft.py` builds the crafted image by patching the audit's exact
inode values into a copy of the range's base disk (`debugfs` + raw inode
surgery, no kernel code involved):

```sh
RANGE=/home/oleg/Projects/pagh-os/attack/range
WORK=/home/oleg/vfs-evidence
python3 tools/security/poc/x86vfs_craft.py --base "$RANGE/base-disk.img" --out "$WORK/vfs.img"
gcc -static -O2 -nostdlib -no-pie -fno-builtin -e _start \
    -o "$WORK/x86vfs_poc" tools/security/poc/x86vfs_poc.c

# fixed kernel: private range copy with the built 2.22.2 kernel swapped in
cp -a "$RANGE" "$WORK/range-fixed"
cp target/x86_64-unknown-none/release/pagh.elf "$WORK/range-fixed/esp/pagh.elf"
cd "$WORK/range-fixed"
for sel in 1 2 3 4 5 6 7 8 9; do
  printf '%s' "$sel" > "$WORK/sel"
  ./run.sh --timeout 120 --disk "$WORK/vfs.img" --inject "$WORK/sel:/sel" \
      --expect 'POC-done' "$WORK/x86vfs_poc"
done
```

The vulnerable contrast is the same loop on an unmodified range copy (stock
2.21.0 kernel, sha256 `0bf8485f…62ffc5a`), with `--expect` set to the marker the
vulnerable side is expected to print instead of `POC-done`.

| sel | finding | 2.21.0 (stock range) | 2.22.2 (fixed) |
|---|---|---|---|
| 1 | F-x86v-1: `lstat` crafted symlink `i_blocks=8, i_size=0xFFFFFFF0` | `[PANIC] … memory allocation of 4294967280 bytes failed` (machine dead) | `POC:newfstatat-noFollow rc=0`, `T1: st_size=0`, `POC-done` |
| 2 | F-x86v-1: `readlink` the same inode | same allocation panic | `POC:readlink rc=-2` (ENOENT) |
| 3 | F-x86v-2: `rename` of a file claiming ~4 GiB | `[PANIC] … memory allocation of 4294963200 bytes failed` | `POC:rename-huge rc=-12` (ENOMEM) |
| 4 | F-x86v-3: uid 1000 opens `/rootdir/secret` (0644 inside root-owned 0700) | `T4: content=ROOT-SECRET-42` (bypass) | `POC:open-rootdir-secret rc=-13` (EACCES) |
| 5 | F-x86v-4: `unlink` a fast/slow-confused symlink whose `i_block[0]` is a live block; statfs free-block delta | `delta=1` — the live block was freed | `delta=0`, `POC:unlink-evil4 rc=0` |
| 6 | F-x86v-5: lookup inside a directory claiming `i_size=0xFFFFF000` | still inside the block scan after the 20 s timeout: `T6: open` printed, no `POC-done`, no panic (a kernel stall — the serial log simply stops); fixed returns immediately | `POC:open-in-bigdir rc=-2` (ENOENT) + `POC-done`, full boot+autorun 17 s |
| 7 | F-x86v-6 (found in this batch): `execve` a crafted regular file claiming ~4 GiB | `[PANIC] … memory allocation of 4294967280 bytes failed` | `POC:execve-bigfile rc=-8` (ENOEXEC) |
| 8 | sanity: valid fast symlink → lstat / readlink / open-through | `st_size=6`, `target=target`, content `TARGET-CONTENT` | same |
| 9 | sanity: create/write/rename/read in `/w` as uid 1000 | works (`rc=0`, content read back) | same |

Selectors 8–9 are the negative control: the fixes do not break normal
symlink/rename/traversal behaviour, and every fixed run ends with the shell
prompt after `POC-done` (the guest program returned). The 2.21.0 panic runs end
in `[PANIC]` with the machine dead (`panic=abort`), which is the vulnerability.

Host-side regressions (`python3 tools/host_tests.py`, CI): the audit's exact
crafted inode values are pinned by
`ext2_links::{crafted_symlink_shape_is_bounded, audit_crafted_symlink_inodes_are_corrupt_not_allocations}`,
by `ext2_bounds::*`, and by
`link_walk_paths::{traversal_requires_search_permission_on_each_directory, search_permission_follows_spliced_link_targets, final_directory_name_needs_only_the_parent_search_bit}`.
