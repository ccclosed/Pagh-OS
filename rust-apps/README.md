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

## CPU OpenGL-1.1 subset: `tinygl`

`rust-apps/tinygl` vendors upstream **TinyGL 0.4.1** (MIT, Fabrice Bellard) and
wraps it in a thin Rust FFI layer. It renders **on the CPU only** into a dense
`0x00RRGGBB` (`XRGB8888`) buffer — the compositor's pixel format — so a client
can draw straight into its memfd and commit damage. PR1–PR3 built the
vendored C + safe FFI (matrices, immediate mode, depth/culling, textures,
display lists); PR5 adds fixed-function lighting and the demo client below.

Honest limits: fixed-function GL 1.1 subset, no shaders/GLSL/GL 2.0+, no
FBO/VBO/`glDrawArrays`, 16-bit Z, `stride == width*4`, `width % 4 == 0`,
one context per process (`gl_ctx` is global), and upstream `gl_fatal_error` is
`exit(1)`. See `tinygl/README.md` for the full list and
`tinygl/UPSTREAM.md` for the pinned pixel digest.

```bash
cd rust-apps/tinygl && cargo test             # criterion6 pixel digest
cargo run --bin tinygl-probe                  # TINYGL PROBE digest/center/corner
cargo build --release --target x86_64-unknown-linux-musl   # needs musl C compiler
```

The musl build needs a C compiler for `x86_64-unknown-linux-musl`
(`apt install musl-tools`, or clang + `TINYGL_MUSL_SYSROOT`); `build.rs` detects
it and prints the install instructions if missing. The ELF is static-pie
(`INTERP` gate: `tinygl-probe --check-interp` plus the existing
`tools/build-rust-app.sh` check).


## CPU TinyGL demo: `pagh-gl-demo`

`rust-apps/pagh-gl-demo` is the PDP/1 client that renders the three CPU scenes
(`triangle`, `cube`, `gears`) **straight into the window's
`mmap(MAP_SHARED)` memfd** through `tinygl`; only `COMMIT` + `DAMAGE` travel
over the socket (PDP/1 `ShmSurface`, no frame copy). Frames are indexed, not
timed: frame `n` has a fixed angle, so FNV-1a pixel digests are reproducible on
the host, in QEMU and on musl. Without `--connect` it runs headless into
`Vec<u32>` and prints `TINYGL <scene> frame=<n> digest=0x…`; with `--connect
/tmp/pagh-desktop.sock` it opens a live window.

```bash
cd rust-apps/pagh-gl-demo && cargo test
cargo run --bin pagh-gl-demo -- gears --hash
tools/build-rust-app.sh rust-apps/pagh-gl-demo   # static-pie musl
```

Honest limits (also in its README): CPU only, fixed-function GL 1.1 subset,
16-bit Z, `stride == width*4`, `width % 4 == 0`, one GL context per process,
and no full-frame fallback — a compositor without `ShmSurface` makes the demo
fail with an explicit error.
