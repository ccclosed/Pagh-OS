# Security policy

pagh is an educational kernel, not a production operating system.

## Threat model in one paragraph

pagh aims at **kernel integrity** (ring 3 cannot read or write kernel memory,
execute writable memory, or touch device MMIO — paging is supposed to enforce it).
That boundary was **violated in 2.21.0**: the three demonstrated ring3-to-ring0
paths were closed in 2.22.0 and the filesystem findings in 2.22.2 (see "Attack
audit" below); the remaining audit findings are listed there and are not claimed
fixed. pagh gives you **package
authenticity** (the TLS/OpenPGP/digest chain above refuses a mirror that cannot prove
what it served), and a **filesystem permission model between processes** (per-process
`uid`/`gid`/`euid`/`egid`/`suid`/`sgid`, POSIX owner/group/other checks on `open`,
`execve`, directory mutation and `kill`, and owner/root-only `chmod`, root-only
`chown`, all against real ext2 inode metadata). It does **not** give you a secure
multi-user system: there is no login or account database, so the boot shell and every
kernel-spawned process run as root and the model constrains only a process that
deliberately drops privileges (`setuid` family). There are no supplementary groups,
ACLs, `fsuid`, capabilities, seccomp, LSM or audit trail, no setuid-binary elevation on
`execve`, no ASLR/KASLR, no KPTI, no SMAP (SMEP is enabled only when CPUID reports it)
and no address-space or mount namespaces. A hostile process that starts as root stays
root; a process that has dropped privileges can still exhaust memory/CPU and has the
whole remaining syscall surface. Side channels, network availability and firmware are
out of scope. Treat it as a teaching kernel that a careful operator runs only with code
they trust. The enforcement points and the exact gaps are listed in
`docs/security-model.md`.

### Credentials and filesystem permissions

Credentials live in `CompatState` (`src/task/compat.rs`) and start as `Creds::ROOT` for
every process the kernel spawns; `fork`/`clone` inherit them unchanged and `execve`
preserves them (setuid *binaries* are not modelled). The pure decisions live in
`src/arch/x86_64/linux/cred.rs` (host property P56), and the syscall layer — not the
VFS — consults them:

* `open`/`openat`: read for `O_RDONLY`, write for `O_WRONLY`/`O_RDWR`, plus write for
  `O_TRUNC`; `O_CREAT` additionally needs write+execute on the parent directory.
* `execve`: at least one execute bit, even for root (Linux's rule).
* `unlink`/`rmdir`/`mkdir`/`rename`: write+execute on every parent directory involved;
  `rename` authorises both parents before it mutates either.
* `chmod`/`fchmod`: root or the inode owner; `chown`/`fchown`: root only. ext2 writes
  `i_mode`/`i_uid`/`i_gid` back through the journal; synthetic nodes (`/dev`, `/proc`,
  ramfs) keep the historical per-process stat override and cannot persist an owner.
* `kill`/`tgkill`: caller's effective uid must be root or equal the target's real or
  effective uid.

Root bypasses read and write, but **not** execute: a `0644` file cannot be execed by
anyone. That rule is what makes the model meaningful even though the default identity
is root.

## Attack audit (2026-11-13): fixed

An external red-team pass ran stock ring-3 Linux ELFs on a private QEMU range
(attachments: PoC sources, serial logs and per-area reports). It found three
independent ring3-to-ring0 paths and a one-syscall machine kill on the 2.21.0
image. **Fixed in 2.22.0** for the ring3-to-ring0 paths and in **2.22.3** for
the user-buffer TOCTOU class (each reproduced before and after; the same PoCs
still succeed on stock 2.21.0):

* **F-LEAD-1 (critical, verified).** `mprotect(2)` accepted kernel virtual
  addresses: no `USER_ADDR_MAX` gate, a coverage guard that inverted to "allowed"
  for a fully mapped kernel range, and `leaf_flags()` (a user-page flag builder
  that always sets `USER_ACCESSIBLE`) re-mapping the found kernel frames. The
  kernel's leaf page tables are shared between the user PML4 and the kernel, so
  this produced a ring-3 W+X alias of kernel memory; scanning the heap for the
  process's credentials and zeroing them gave uid 0. `mprotect`/`munmap`/`mremap`
  now share a user-range gate, the `PROT_NONE` path can free only user frames, and
  `vmm::map` refuses `USER_ACCESSIBLE` kernel mappings.
* **F-LEAD-2 (critical, verified).** A forged `rt_sigreturn(2)` frame restored
  user-controlled `eflags` into `r11`; `sysretq` loaded them as RFLAGS, so
  `IOPL=3` gave ring 3 I/O ports and PCI config space (virtio-blk BAR observed).
  Frame decode now rejects `ip`/`sp` outside the user half and masks restored
  flags to `USER_RFLAGS_MASK` (IOPL/NT/VM/RF/AC cleared); the TSS carries no I/O
  bitmap, so ports need both this mask changed and a new TSS to become reachable.
* **F-x86a-1 (critical, verified).** `recvfrom(2)` wrote the source `sockaddr`
  and `socklen_t` through unvalidated user pointers: 16 bytes plus a constant 4
  at two arbitrary addresses, proven against the kernel tick counter (a one-shot
  arbitrary kernel write, and with a known `Creds` address a direct privesc).
  Both outputs are now `check_user_ptr`-validated before the write.
* **F-x86a-3 (medium, verified).** `connect(2)` read `sa_family` through the
  caller's pointer after checking only the *length*, so a non-canonical pointer
  caused a ring-0 #GP and the fault handler halted the machine. The pointer is
  now validated before the read.
* Also in 2.22.0: the EINVAL syscall diagnostic (attacker-triggerable, flooded
  serial and framebuffer from the exploit loop) is rate-limited to one line per
  second.
* **F-x86a-4 (medium, fixed in 2.22.3).** `poll`/`ppoll`, `select`/`pselect6`
  and `epoll_wait` validated their user-visible array once and then kept
  publishing into it after a `yield`; a sibling `CLONE_VM` thread could munmap
  (or `mprotect PROT_NONE`) that page in the window, so the next ring-0 access
  faulted in kernel mode and halted the machine. The wait loops now work on
  private kernel copies, re-validate the user range after every yield and
  return `EFAULT` when it vanished — and the same "validate, block, publish"
  pattern is closed in the read family, the `readv`/`writev` iovec entries,
  `wait4` and `sendfile`. `clone`'s `parent_tid`/`child_tid` writes are
  validated and written inside the interrupt-disabled spawn/fork window (a bad
  pointer is `EFAULT` before the child exists), `ioctl(TCSETS*)` honours its
  validation result (it used to read anyway), and a kernel-mode page fault on a
  user address now kills only the faulting compat process instead of the
  machine (kernel-address and NULL faults stay fatal). PoC:
  `tools/security/poc/poc_poll_toctou.c` (`poll`/`ppoll`/`select`/`epoll`/
  `futex` modes).
* **F-x86a-6 (medium, found in review, fixed in 2.22.3).** `sendto(2)` and
  `recvfrom(2)` validated the caller's payload with a capped length
  (`count.min(1 MiB)`) but then allocated/copied the RAW `u64` count: 1 MiB of
  mapped pages was enough to make the kernel request a ~4 GiB allocation, which
  aborts the kernel on a 1 GiB guest (the crafted-ext2 `i_size` class, F-x86v-1,
  through one socket syscall). `abi::socket_payload_len` now refuses an
  oversized `sendto` with `EMSGSIZE` before allocating and truncates
  `recvfrom`'s capacity to the 1 MiB cap (the buffer is validated, allocated and
  copied at exactly that length). `sendmsg`/`recvmsg` are not implemented in
  this layer, so the pattern has no second copy. PoC:
  `tools/security/poc/poc_sock_large_count.c`.

**Fixed in 2.22.5 (round-2 audit):**

* **F-r2-vfs-1 (high, verified).** A hard-link alias of an archive symlink
  bypassed both F-x86n-1 layers. `link(2)` does not follow the final component,
  so `./d/y -> ../etc` + `./d/h` HARDLINK to `./d/y` created a second name for
  the *symlink inode*; the planner only knew `d/y` as a link and the effectful
  resolver followed the alias, so `./d/h/autorun` (HARDLINK to `./d/pwn`) was
  written to `/mnt/etc/autorun` and executed as root at the next boot. The pure
  planner now computes the hard-link/symlink equivalence closure up front
  (aliases of aliases and `..`-normalized targets included, archive order
  irrelevant), reports the alias with `UnsafeKind::HardlinkAliasesSymlink` and
  every member below it, and the installer refuses the whole package
  (`InstallError::UnsafePath`) before writing anything. The effectful layer
  never creates a second name for an archive symlink and never traverses a
  symlink with `nlink > 1`, which also covers an unmarked alias left by an
  earlier install or a planner regression. Evidence: the audit's
  `attack/r2/vfs/attack-alias.deb` refused as a whole with `/mnt/etc/autorun`
  absent and a clean next boot, the new `tar_links` host properties, the
  in-QEMU `install_hardlink_alias_confinement` selftest, and the extended
  `tests/pkg/e2e_pkg_escape.py` (round-1 packages still refused, a normal `.deb`
  with a regular-file hard link still installs).

**Fixed in 2.22.1 (this branch):**

* **F-x86n-1 (high, verified).** A `.deb` carried a symlink member pointing
  outside the installation root (`./y -> /etc`), and later archive members
  resolved their parents through it: `./y/autorun` passed the `..` check and was
  written to `/mnt/etc/autorun`, executed as root at the next boot
  (`./x -> /` + `./x/pwned_root` wrote `/mnt/pwned_root` the same way). Two
  independent layers now refuse it: the pure planner drops any member or
  hard-link target whose ancestor component is a symlink the same archive
  creates, and `install_data_tar` refuses the whole package
  (`InstallError::UnsafePath`) before creating an inode; the effectful parent
  resolver starts at the install-root node, never follows a package-created or
  absolute-target link (the guest namespace maps `/etc` to `/mnt/etc`, which is
  exactly the escape), keeps every `..` and spliced relative target under the
  root, and resolves hard-link targets through the same walker. A pre-existing
  *relative* in-root link (`lib64 -> usr/lib64`) is still followed. Evidence:
  F-x86n-1 host properties, the in-QEMU `install_symlink_confinement` selftest,
  and the committed live harness `tests/pkg/e2e_pkg_escape.py` (the audit's
  crafted `.deb` refused with the offending path named, `/mnt/etc/autorun`
  absent, a normal `.deb` still installs).

**Fixed in 2.22.2** (the x86 VFS/ext2 batch; each reproduced on
the stock 2.21.0 range and re-run against the fixed kernel):

* **F-x86v-1 (medium, verified).** Crafted ext2 symlink inodes with a huge
  `i_size` made `read_symlink()` allocate `vec![0; i_size]` (4 GiB) →
  allocation panic, machine dead. The target shape is now classified from the
  inode fields before allocation: fast only for a genuinely inline target
  (`i_blocks == 0`, ≤ 60 bytes), slow only for a one-block data link
  (≤ 4096 bytes), everything else `FsError::Corrupt`.
* **F-x86v-3 (medium, verified).** The path walker required no search (`x`)
  permission on intermediate directories, so uid 1000 could read a 0644 file
  inside a root-owned 0700 directory. The pure link walker now asks
  `may_search` before every lookup; the syscall layer answers with
  `Creds::may(..., Access::Execute)` of the effective ids (`access(2)` with its
  real ids), including components spliced in from a followed link target, and
  maps a denial to `EACCES`. `chdir(2)` also needs search on the target
  directory itself, and `execve`'s image read uses the checked walk.
* **F-x86v-2 (static).** `rename`'s copy emulation allocated the source
  `i_size` in one `Vec`; a sparse ~4 GiB file aborted the kernel. The copy is
  capped at 64 MiB (`ENOMEM` above) and reserved fallibly.
* **F-x86v-4 (static).** Fast/slow symlink confusion on `unlink`: an
  `i_blocks == 0, i_size > 60` inode took the slow branch and freed its target
  text bytes as a block number. Deletion now decides from `i_blocks` alone;
  the read path reports that shape as corrupt.
* **F-x86v-5 (static).** Crafted directory `i_size` made one lookup/readdir
  scan up to ~1M blocks (kernel stall). Directory sizes are validated
  (block-aligned, not larger than the filesystem) before the loop.
* **F-x86v-6 (found while fixing the batch).** The `execve`/`PT_INTERP` image
  reader had the same unbounded `i_size` allocation and an unchecked exec
  walk. Image size is capped at 256 MiB with a fallible reserve, and the exec
  path is credential-checked.

**Fixed in the sibling repositories** (not this kernel; each has its own
release notes, PoC and live transcript):

* **F-rv-1/2/3 (RISC-V kernel, pagh-riscv 0.24.1, PR #12).** Crafted-ext2
  symlink `i_size` is bounded before allocation (fast ≤ inline capacity, slow ≤
  one block, else `FsError::Corrupt`); overlapping `PT_LOAD` segments that would
  merge to W+X are rejected with `ElfError::WxOverlap`; `mprotect` validates
  that it writes the process's own page-table root (`protect_in`), not whichever
  root is active. Evidence: crafted ext2/W^X ELFs refused in live QEMU/OpenSBI,
  `cat` of the crafted link prints "filesystem is corrupt" and the machine
  stays up, regression `sv39-map-roundtrip`/reject matrix.
* **F-rt-1/2/3 (pagh-libc 0.15.1, PR #9).** `ld-pagh` no longer publishes
  `__pagh_dl_state` through an untrusted self-scope `SHN_ABS` symbol and rejects
  out-of-image symbol/`PT_DYNAMIC` values as malformed; `vsnprintf` clamps
  integer precision and writes zero-extension directly into the output field
  (no `memmove` into the 32-byte stack scratch), with a host-differential test
  against glibc (1.3M checks) and an ASAN regression.
* **F-rt-4 (pagh-python 0.9.1, PR #6).** Lexer and parser enforce a nesting
  limit (CPython's 200) on brackets and on non-bracket recursion; pagh-riscv
  additionally guards stack headroom. The audit's 506-byte 200-paren PoC now
  fails with `SyntaxError: maximum recursion depth exceeded` instead of
  exhausting the stack.

No finding from the 2026-11-13 audit remains unaddressed. Two new findings were
made while fixing this batch and are fixed in the same releases: F-x86v-6
(unbounded `execve` image allocation, 2.22.2) and F-x86a-6 (`sendto`/`recvfrom`
payload count bypassing the validated length cap, 2.22.3).

Known structural limitations the audit reconfirmed: **no KASLR** (heap base and
kernel symbols are fixed, which is why the PoCs hardcode/scan addresses), **no
SMAP**, SMEP only when CPUID reports it, shared kernel leaf page tables between
address spaces, and **no authenticated boot**. The login/accounts feature planned
in `AUTH.md` does not change this: it protects against casual use, not against a
process that can still reach ring 0.

## Safe defaults

The development build enables outbound package downloads through the default `network_packages` Cargo feature, which activates the network transport; `cargo build --no-default-features` produces a fail-closed build with no outbound package transport.

**TLS peer authentication is implemented (issue #14, PRs #22–#29 of the series):** an HTTPS fetch validates the server certificate chain down to the committed CA bundle (`src/net/ca_bundle.rs`, roots pinned by sha256 at generation time), authorizes the connection host against the leaf's SAN entries (RFC 6125, no CommonName fallback), enforces the validity windows, refuses to validate at all when the system clock is unset (below 2025-01-01), and checks the handshake `CertificateVerify` signature against the verified leaf key. A failed check aborts the handshake — there is no fallback that keeps the connection, and the verifier's `error!` diagnostics (`Package_Fetcher(tls): stage=verify cause=…`) name the refused check.

**A handshake that skips the certificate is refused too.** RFC 8446 §4.4.2.4 requires a client to reject a server `Finished` that was not preceded by `Certificate` and `CertificateVerify`. Upstream `embedded-tls` did not enforce that, so a peer could omit both messages, compute a valid `Finished` (an active MITM that terminates the key exchange itself can), and reach application data with the verifier never invoked — a fail-open path that no amount of verifier logic could close. This tree therefore carries a local patch to `vendor/embedded-tls/src/connection.rs` that enforces the ordering (PSK handshakes, which legitimately send no certificate, are exempt), and `src/net/tls.rs` independently requires its authentication counter to advance across the handshake before a single application byte moves. Both are deliberate: the patch is the protocol-correct fix, the counter keeps the guarantee if the vendored crate is ever refreshed.

**`AT_RANDOM` is best-effort when hardware entropy is missing (issue #16).** glibc reads the ELF `AT_RANDOM` block unconditionally at process start, so — unlike `getrandom`, which returns `EAGAIN` — it cannot fail closed: refusing would mean refusing to start the process. It prefers RDSEED/RDRAND; when the CPU exposes neither, the block is derived from a mixed boot seed (SHA-256 over TSC-delta jitter, CMOS RTC port-read latency/races, kernel counters and address-space layout) plus a per-process counter. **Only the timing sources carry unpredictability**: the tick clock, the pid, the PMM/heap counts, CR3 and object addresses are observable or fixed by the configuration — a five-boot measurement found them constant (0 bits) and all the variation in the RTC read latencies and the TSC-delta fold. The seed is therefore not a function of the public inputs (that is what the previous xorshift fallback was), but its strength rests on timing jitter alone, and that is claimed rather than proven. This is **best-effort, not a CSPRNG**: on a fully deterministic replay the collected timing is reproducible and so are the canaries. The degradation is therefore announced once at boot (`entropy: stage=degraded cause=NoHardwareEntropy cpu=…`) instead of being silent, and booting QEMU with `-cpu max` (RDSEED/RDRAND present) stays the recommended configuration. Host properties P51 prove the mixer's avalanche and secret separation and reject the removed xorshift plus degenerate mixers as negative controls; the in-guest `selftest` samples 64 blocks and prints a digest, which is what makes a `-cpu qemu64` run comparable across boots.

### Trust is limited to the pinned roots

The bundle trusts exactly four self-signed roots: **ISRG Root X1**, **ISRG Root X2**, **GTS Root R1**, and **GTS Root R4** (each selected by a sha256 pin in `tools/gen_ca_bundle.py`). Consequences:

* An HTTPS mirror whose chain does not reach one of those four roots is **refused** (`ChainError::NoAnchor`), even if the certificate is otherwise perfectly valid and issued by a widely trusted CA. Adding a root means deliberately re-running the generator with a reviewed pin and committing the regenerated `src/net/ca_bundle.rs`.

* Plain-HTTP mirrors (`apt setmirror http://…`) are **unauthenticated at the transport layer**: cleartext, no certificate to check. What carries that path instead is the OpenPGP metadata chain below — the signature over `InRelease`/`Release.gpg`, the SHA-256 of `Packages`, and the SHA-256 of every `.deb` — so a cleartext mirror can no longer substitute metadata or payloads, although it can still observe and deny service. It is no longer the live default either: `deb.debian.org` is reached over HTTPS, large indexes included, and the `lx_livetest` harness refuses to run on a cleartext config (issue #19).

* Certificate **revocation is not checked** (no CRL, no OCSP, no stapling): a certificate that its issuer has revoked but that is still inside its validity window is accepted.

### Repository metadata is verified against pinned Debian keys

`apt update` refuses to use anything the signature chain does not cover (issue #32):

* `dists/<suite>/InRelease` is verified as a clear-signed OpenPGP message, or — only when it is absent — `Release.gpg` against `Release`, against the keyring committed in `src/pkg/openpgp_keys.rs`: the Ed25519 *Debian Stable Release Key (13/trixie)* and the RSA-4096 archive keys *13/trixie* and *12/bookworm*, each pinned by its v4 fingerprint, its signing subkeys bound by verified key binding signatures, with expiry, revocation and key flags enforced and the 2025 clock gate applied.
* The `SHA256:` entry of the verified `Release` is the authority for the `Packages` variant that follows: the downloaded body must match both digest **and** size before it is decompressed or parsed.
* Every downloaded `.deb` must match the `SHA256:`/`Size:` of the stanza that named its `Filename:` — checked after the download and **before** the payload is parsed, decompressed or written.
* A mirror that serves no signatures is **refused with a visible error**, and there is no flag, feature or environment variable that turns verification off. The only unverified build is `cargo build --no-default-features`, where `apt update`/`install` return `NetworkDisabled`.

Deliberate strengthening over `gpgv`: a signature made by an **expired** key is refused (`gpgv` verifies it and exits 0), and partial-length packets are refused rather than reassembled.

### What is still not verified

* **Revocation distribution:** a revocation is honoured only if the revocation signature is inside the committed keyring block. There is no CRL/OCSP-equivalent refresh, so a key revoked upstream is learned only by shipping a regenerated keyring.
* **Replay of a complete older triplet** (`Release` + `Packages` + `.deb`) is **not detected** on suites without `Valid-Until` — which includes `stable`; only suites that carry `Valid-Until` (e.g. `security`) are covered by that check, plus the future-dated-signature checks. The E2E harness *demonstrates* this rather than leaving it implied: the local mirror's `stale` suite is a complete, correctly signed older revision (older `Date`, older package version), `apt update` accepts it, and the run prints `LXSELFTEST apt_verify stale-rollback NOTE (accepted: 1.0 -> 0.9 …)`. Closing it needs either `Valid-Until` on every suite, persistent "highest release seen" state (none exists: the index is RAM-only), or an authenticated freshness channel — none of which is in scope here.
* **Which refusal paths are proven where.** The QEMU E2E harness proves, against the real `apt` pipeline: a correctly signed mirror loads and installs (positive control), and four refusals — a `Packages` body that does not match the signed `Release` **while its signature verifies**, a `.deb` that does not match the signed index (nothing written to `/mnt`), a mirror with no signatures, and a mirror signed by an unpinned key; each refusal also leaves no index published. Refusals that need a guest clock or a Debian signing key are covered by **host properties only, not end-to-end**, and are not claimed as E2E: `ClockUnset` (P52), key expiry/revocation/not-yet-valid (P52/P53), future-dated signatures and `Date`/`Valid-Until` parsing (P52/P54), `NoIndexEntry` (P54), and the ECDSA path (P52, synthetic keys — no Debian archive key uses ECDSA today). These properties run **the same verifier** the kernel compiles; what they do not exercise is the apt plumbing around it.

The E2E *tool* for the lifecycle branches already exists and is validated: `tools/gen_openpgp_testkey.py` emits two deterministic lifecycle keys (one whose window closed an hour after creation, one created ten years in the future) and host property P53 proves the verifier refuses them as `Expired` / `NotYetValid`. They are deliberately **not** listed in the harness anchor (`TEST_TRUST_ANCHORS`) and not wired into an E2E scenario — the branches are already covered by the properties above, and adding them to the E2E run would widen the surface the verifier has to converge on without adding evidence.
* **TLS revocation** is still unchecked (no CRL/OCSP/stapling) and trust is limited to the four pinned roots.
* **`pkg <host> <path>`** (the manual, index-free `.deb` downloader) remains unauthenticated by construction; `apt` is the verified path.
* **`Acquire-By-Hash`** is not used: pagh fetches the plain index path, which is checked against the same signed entry.
* The ECDSA verification path exists but no real Debian key exercises it yet (all current archive keys are RSA or Ed25519), so it is covered only by synthetic host tests.

## Reporting

Report memory-safety, privilege-boundary, filesystem-integrity, parser, or package-supply-chain issues privately to the repository maintainers. Include a minimal reproducer, build profile, QEMU version, and serial log.

## Release requirements

A security-capable package manager requires all of the following before the demo feature can become a default:

1. ~~CSPRNG-backed TLS randomness.~~ **Done for TLS and `getrandom`** — RDSEED/RDRAND, fail-closed when unavailable. The one documented exception is ELF `AT_RANDOM` (issue #16): glibc reads it unconditionally, so it cannot fail closed; it uses the mixed boot seed, warns once at boot when hardware entropy is missing, and is best-effort by construction.
2. ~~Certificate-chain, hostname, and expiry validation.~~ **Done, with two named gaps** — `net::tls_auth` / `net::tls_chain` / `net::ca_bundle`, driven by the handshake verifier in `net::tls`, fail-closed, and the "no certificate at all" bypass is closed in `vendor/embedded-tls` + the `VERIFIED_HANDSHAKES` gate. Not covered: **revocation** (no CRL/OCSP/stapling) and **root agility** (trust is the four pinned roots above; any other issuer is refused). Both are documented in "Safe defaults" rather than implied by "Done".
3. ~~Signed repository metadata verification.~~ **Done** — the OpenPGP chain above, pinned by fingerprint. Not covered: revocation distribution and replay of a whole old triplet (both named in "What is still not verified").
4. ~~Digest verification of `Packages` and every `.deb` before parsing or installation.~~ **Done** — digest **and size**, checked before decompression/parsing and before any unpacking.
5. Negative integration tests for MITM, corrupted metadata, digest mismatch, path traversal, malformed archives, and malformed ELF files. **Partly done.** The apt side is now proven in-QEMU by one reproducible command (`python3 tools/e2e.py local-mirror`, whose `LXSELFTEST apt_e2e PASS` is gated on it): a correctly signed local mirror loads and installs, while a **valid signature over a `Packages` file that was replaced afterwards** (equal size, different SHA-256), a **tampered `.deb`** (equal size, different SHA-256, nothing written), a mirror with **no signatures at all**, and a mirror signed by an **unpinned key** are each refused with a named diagnostic (`apt: verify FAIL stage=… cause=…`). The signed test mirror uses a deterministic test key whose anchor is compiled only into the `lx_selftest`/`lx_bigindex` harness builds (`src/pkg/openpgp_test_keys.rs`); a production build cannot trust it. Still missing from this item: the certificate-omission MITM regression test (needs a TLS server that deliberately skips `Certificate`, which rustls will not do) and the path-traversal/malformed-archive cases, which remain covered by host properties and code review.
