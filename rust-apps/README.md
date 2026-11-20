# Rust applications for pagh

pagh runs statically linked `x86_64-unknown-linux-musl` Rust executables through its ring-3 Linux compatibility layer (dynamically linked glibc binaries also run via `lxrun`).
The Linux layer implements threads (`CLONE_VM|CLONE_THREAD`, including musl's
`CLONE_DETACHED`), a real address-space `fork` (`fork_linux_process`), `posix_spawn`
(`clone(CLONE_VM|CLONE_VFORK)` as a COW fork plus parent wait), `futex`, and POSIX signal
delivery — so a Rust program that spawns threads, child processes or handles signals
should work; the samples here stay single-threaded on purpose. GUI libraries are not supported: there is no X11/Wayland
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
chrome (for client-side decorations), a PNG encoder, and the PDP/1 protocol client
used by `pagh-desktop`. `rust-apps/pagh-files` is the Files application built on
top of it (sidebar, grid/list views, context menu with the Dolphin-style
"Создать" submenu, properties dialog, VFS trait plus a fake and a host backend).
Files draws only the client area — the compositor owns the title bar, panel and
window management — and asks the compositor to launch the terminal through the
PDP/1 `launch` (type 13) extension. Both are dependency-free crates with their
own `[workspace]` and `.cargo/config.toml` (the root config targets bare metal),
so they build for the host or for static musl:

```bash
cd rust-apps/pagh-ui && cargo test && cargo run --bin pagh-ui-gallery -- --out screenshots
cd rust-apps/pagh-files && cargo test && cargo run -- --out screenshots
cargo build --release --target x86_64-unknown-linux-musl   # in either crate
```

Demos and screenshot tests render the same deterministic scenes; the end-to-end
test in `pagh-files/tests/pdp_e2e.rs` runs the real `pagh-desktop` compositor and
the real Files binary over an `AF_UNIX` socket. See `pagh-ui/README.md`,
`pagh-files/README.md`, `pagh-ui/PROTOCOL.md` and `pagh-desktop/PROTOCOL.md`.
