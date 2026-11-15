# Rust applications for pagh

pagh runs statically linked `x86_64-unknown-linux-musl` Rust executables through its ring-3 Linux compatibility layer (dynamically linked glibc binaries also run via `lxrun`).
The Linux layer implements threads (`CLONE_VM|CLONE_THREAD`), a real address-space `fork`
(`fork_linux_process`), `futex`, and POSIX signal delivery — so a Rust program that spawns
threads or handles signals should work, but this sample (and the supported targets) are
deliberately single-threaded. GUI libraries are not supported: there is no X11/Wayland
server, only the kernel's own framebuffer.

Build the sample:

```bash
tools/build-rust-app.sh rust-apps/hello
```

The ELF is written to `rust-apps/out/pagh-rust-hello`. Copy it to the ext2 data image (for example `/mnt/pagh-rust-hello`) and run inside pagh:

```text
rust /mnt/pagh-rust-hello one two
```

Programs should remain single-threaded and use ordinary file/stdio/time/allocator APIs. Unsupported Linux syscalls return `ENOSYS` and are logged once per process.

## Desktop B: `pagh-ui`, `pagh-files` and `pagh-desktop`

`rust-apps/pagh-ui` is the client UI toolkit for the userspace desktop (variant B):
software RGBA canvas, embedded bitmap font, theme, widgets, context menus, window
chrome, a PNG encoder, and the PGUI v1 protocol client. `rust-apps/pagh-files` is
the Files application built on top of it (sidebar, grid/list views, context menu
with the Dolphin-style "Создать" submenu, properties dialog, VFS trait plus a
fake and a host backend). Both are dependency-free crates with their own
`[workspace]` and `.cargo/config.toml` (the root config targets bare metal), so
they build for the host or for static musl:

```bash
cd rust-apps/pagh-ui && cargo test && cargo run --bin pagh-ui-gallery -- --out screenshots
cd rust-apps/pagh-files && cargo test && cargo run -- --out screenshots
cargo build --release --target x86_64-unknown-linux-musl   # in either crate
```

Demos and screenshot tests render the same deterministic scenes; see
`pagh-ui/README.md`, `pagh-files/README.md` and `pagh-ui/PROTOCOL.md`.
