# Rust applications for pagh

pagh runs statically linked `x86_64-unknown-linux-musl` Rust executables through its ring-3 Linux compatibility layer (dynamically linked glibc binaries also run via `lxrun`).
The Linux layer implements threads (`CLONE_VM|CLONE_THREAD`, including musl's
`CLONE_DETACHED`), a real address-space `fork` (`fork_linux_process`), `posix_spawn`
(`clone(CLONE_VM|CLONE_VFORK)` as a COW fork plus parent wait), `futex`, and POSIX signal
delivery — so a Rust program that spawns threads, child processes or handles signals
should work; the samples here stay single-threaded on purpose. There is still no X11/Wayland
server: the GUI is the userspace desktop below (variant B) plus the kernel's own framebuffer,
and a program draws into its `memfd` through the PDP/1 toolkit rather than through a system
GUI library.

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
PDP/1 `launch` (type 13) extension. `rust-apps/pagh-terminal` is that second client: a
VT100/ANSI emulator with its own renderer, a PDP/1 client and an `ShmSurface` path
(memfd handed over with `SCM_RIGHTS`, damage rectangles on the socket), and it is also the
consumer of the kernel terminal session (`/dev/vtfb1` + `/dev/vtinput1`, see
`docs/terminal-session.md`). All three are dependency-free crates with their own
`[workspace]` and `.cargo/config.toml` (the root config targets bare metal),
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
can draw straight into its memfd and commit damage. Status: **PR1 build-probe + PR2 FFI
core, headless.** PR1 proved upstream C 0.4.1 builds for musl, links static-pie and produces
the reference pixels (a strict digest); PR2 adds the safe layer (context, viewport/clear,
matrix stack, immediate-mode primitives, depth/cull, `ZB_resize`). No desktop demo and no
compositor integration yet — that is the next PR, and the system build (`build-rust-app.sh`)
is still unchanged.

Honest limits: fixed-function GL 1.1 subset, no shaders/GLSL/GL 2.0+, no
FBO/VBO/`glDrawArrays`, 16-bit Z, `stride == width*4`, `width % 4 == 0`,
one context per process (`gl_ctx` is global), and upstream `gl_fatal_error` is
`exit(1)` — every invariant is validated by our `Surface`/`Context` before C is entered.
See `tinygl/README.md` for the full list and
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
