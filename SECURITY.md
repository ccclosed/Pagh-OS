# Security policy

pagh is an educational kernel, not a production operating system.

## Threat model in one paragraph

pagh aims at **kernel integrity** (ring 3 cannot read or write kernel memory,
execute writable memory, or touch device MMIO — paging is supposed to enforce it).
That boundary was **violated in 2.21.0** and the three demonstrated ring3-to-ring0
paths were closed in 2.22.0 (see "Attack audit" below); the remaining audit findings
are listed there and are not claimed fixed. pagh gives you **package
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

## Attack audit (2026-11-13): fixed and still open

An external red-team pass ran stock ring-3 Linux ELFs on a private QEMU range
(attachments: PoC sources, serial logs and per-area reports). It found three
independent ring3-to-ring0 paths and a one-syscall machine kill on the 2.21.0
image. **Fixed in 2.22.0** (each reproduced before and after; the same PoCs still
succeed on stock 2.21.0):

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

**Still open from the same audit** (no claim that they are fixed; next batches):

* **F-x86n-1 (high, verified).** A `.deb` may carry a symlink member that points
  outside the installation root; later archive members resolve through it, so a
  package installs `/mnt/etc/autorun` and the shell executes it as root on the
  next boot. Fix: confine symlink targets to the install root and resolve parents
  without following package-created links.
* **F-x86v-1 / F-rv-1 (medium, verified).** Crafted ext2 symlink inodes with a
  huge `i_size` make `read_symlink()` allocate `vec![0; i_size]` (4 GiB) →
  allocation panic, machine dead. Fix: bound symlink sizes (fast inline; slow
  ≤ one block) and report `Corrupt`.
* **F-x86v-3 (medium, verified).** The path walker does not require search
  (`x`) permission on intermediate directories, so uid 1000 can read a 0644 file
  inside a root-owned 0700 directory. Fix: `Access::Execute` on every component.
* **F-x86a-4 / F-x86v-2/4/5 / F-rv-2/3 (static).** TOCTOU on user buffers that
  are validated once and then used across a `yield`; `rename` copying a sparse
  `i_size`; fast/slow symlink confusion; RISC-V W^X flag merge and `mprotect`
  root confusion. To be reproduced, then fixed.
* **F-rt-1…4 (RISC-V userland runtime, high/medium/low, verified).** `ld-pagh`
  symbol/`PT_DYNAMIC` bounds (arbitrary write / OOB read), `pagh-libc`
  `vsnprintf` precision overflow (stack smash), `pagh-python` parser recursion
  (stack exhaustion). These live in `pagh-libc`/`pagh-python`, not this kernel.
* **F-rt-4 and the RISC-V items** are tracked in those repositories' own
  `SECURITY`/report files; the x86_64 kernel audit items above are the kernel's.

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
