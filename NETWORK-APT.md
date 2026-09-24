# Network apt in pagh

The normal pagh development build enables network package operations by default through the `network_packages` Cargo feature.

```bash
./run.sh --release
```

Inside pagh:

```text
ifconfig
apt setmirror https://deb.debian.org /debian
apt update
apt install busybox-static
lxrun /mnt/bin/busybox
python            # CPython 3.13, installed by first-boot provisioning
```

## First-boot provisioning

On the first boot with a fresh `disk.img`, the kernel seeds `/mnt` (release metadata, home
skeleton, mini-Rust example). Installing the base glibc + CPython 3.13 userland (≈35 packages,
≈50 MB of downloads — the equivalent of `apt update && apt install python3`) is **opt-in**: the
boot asks `[Y/n]` on the console and only a `Y`/Enter starts the background provisioning thread.
It is idempotent — on later boots it detects the installed userland and exits. Progress is
reported on serial; the framebuffer log mirror is paused while it runs so the shell stays usable.
Delete `disk.img` to re-provision from scratch.

## Index decode robustness

`apt update` tries the `Packages.gz`, `Packages.xz`, and plain `Packages` variants in
order — but only those the signed `Release` lists, with the digest and size it declares.
A decode failure in one variant (e.g. a corrupt gzip stream) logs an honest
`deb:`/`apt:` diagnostic on serial and falls through to the next variant instead of

aborting; a **digest or size mismatch** does not fall through — it aborts the update,
because a mismatch means the mirror served bytes that differ from the signed metadata. Package payloads unpack with tar symlinks/hardlinks materialized as file
copies, since the ext2 writer has no symlink support.
aborting. Package payloads unpack with tar symlinks and hardlinks **created as real
links**: symlink inodes and shared inodes through the ext2 writer (issue #18), with
the `tar` `'1'`/`'2'`/GNU-long-name/pax-prefix forms parsed and no silent
truncation of paths over 100 bytes.


## Trust status

DNS, HTTP/HTTPS, package-index parsing, dependency resolution, `.deb` decompression and ext2 installation are implemented. HTTPS authenticates the mirror fail-closed: the server chain must reach the committed CA bundle (four pinned roots — ISRG Root X1/X2, GTS Root R1/R4), the leaf SAN must authorize the host, the 2025 clock gate must pass, and the TLS 1.3 `CertificateVerify` signature must check out; a handshake that omits the certificate is refused outright. Plain HTTP is unauthenticated as a transport, but it no longer decides what pagh trusts: `apt update` verifies `InRelease`/`Release.gpg` against the pinned Debian keys, checks the `Packages` digest **and** size from the signed `Release` before parsing it, and `apt install` checks each `.deb` digest **and** size from the signed index before unpacking it. A mirror that serves no signatures is refused outright (no override flag exists). Deployment consequences: the local mini-repo used by the E2E harnesses must be signed with the test anchor, and a mirror that is not signed by one of the pinned Debian keys is unusable by design; what remains unverified (revocation distribution, replay of a whole old triplet on suites without `Valid-Until`, the index-free `pkg` command) is stated in `SECURITY.md`.

For a fail-closed build:

```bash
cargo build --no-default-features
```
