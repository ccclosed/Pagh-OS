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
