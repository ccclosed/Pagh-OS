# Security regression PoCs (2026-11-13 attack audit)

Committed copies of the exploit sources from the red-team report, so the fixes
they motivated can be re-run later. The boot range itself (a frozen ESP, base
disk, `cc-static`, `run.sh`) lives **outside this repository**; point `RANGE` at
it. Nothing here builds into the kernel or runs in CI.

```sh
RANGE=/home/oleg/Projects/pagh-os/attack/range   # private range from the report
```

## What each PoC proves

| File | Finding | Vulnerable 2.21.0 | Fixed (2.22.0) |
|---|---|---|---|
| `kheap.c` | F-LEAD-1: `mprotect()` on kernel VAs → ring-3 W+X alias of the kernel heap → zero the process's `Creds` → uid 0 | `PWN-KHEAP: SUCCESS root file contents: ROOT-ONLY-SECRET-7f3a` | `PWN-KHEAP: mprotected pages=0x0` + `creds pattern not found` (uid stays 1000) |
| `iopl.c` | F-LEAD-2: forged `rt_sigreturn` frame sets `RFLAGS.IOPL=3` → ring 3 I/O ports / PCI config | `PWN-IOPL: RFLAGS=0x…3202` then `PWN-IO: port 0x80`, `PWN-IO: pci dev=3 id=0x10011af4` | `PWN-IOPL: RFLAGS=0x…0202`, dies on the first port access; no `PWN-IO:` lines |
| `poc_connect_gp.c` | F-x86a-3: `connect()` reads `sa_family` through an unvalidated pointer → ring-0 #GP → machine halt | no shell prompt after `[autorun]` (machine halted) | `POC-CONNECT-GP: SURVIVED r=-1 errno=14` (EFAULT), shell prompt returns |
| `poc_recvfrom_write.c` | F-x86a-1: `recvfrom()` writes `sockaddr`/`socklen_t` through unvalidated pointers (16-byte arbitrary kernel write) | `PWN-RECVFROM: arbitrary kernel write confirmed: MONOTONIC jumped …` | the validated write path refuses, so no datagram is reported as received; `PWN-RECVFROM` never prints |

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

These four findings were the 2.22.0 batch. The audit's other items (package
symlink escape, crafted-ext2 sizes, `walk()` search permissions, TOCTOU, RISC-V
runtime) are listed in `SECURITY.md` and do not have a committed PoC here yet.
