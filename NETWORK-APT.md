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

On the first boot with a fresh `disk.img`, a background kernel thread seeds `/mnt`
(release metadata, home skeleton, mini-Rust example) and then runs the equivalent of
`apt update && apt install python3`, laying down the base glibc + CPython 3.13 userland
(≈35 packages, ≈50 MB of downloads). The thread is idempotent — on later boots it
detects the installed userland and exits. Progress is reported on serial; the
framebuffer log mirror is paused while it runs so the shell stays usable. Delete
`disk.img` to re-provision from scratch.

## Index decode robustness

`apt update` tries the `Packages.gz`, `Packages.xz`, and plain `Packages` variants in
order. A decode failure in one variant (e.g. a corrupt gzip stream) logs an honest
`deb:`/`apt:` diagnostic on serial and falls through to the next variant instead of
aborting. Package payloads unpack with tar symlinks/hardlinks materialized as file
copies, since the ext2 writer has no symlink support.

## Trust status

DNS, HTTP/HTTPS, package-index parsing, dependency resolution, `.deb` decompression and ext2 installation are implemented. TLS 1.3 encrypts traffic, but CA/hostname verification, Debian `InRelease` signature validation and package SHA-256 verification are not complete. apt displays this warning before network use.

For a fail-closed build:

```bash
cargo build --no-default-features
```
