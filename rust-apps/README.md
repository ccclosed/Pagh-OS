# Rust applications for pagh

pagh runs statically linked `x86_64-unknown-linux-musl` Rust executables through its ring-3 Linux compatibility layer (dynamically linked glibc binaries also run via `lxrun`). Threads, `fork`, futexes, signals, and GUI libraries are not supported.

Build the sample:

```bash
tools/build-rust-app.sh rust-apps/hello
```

The ELF is written to `rust-apps/out/pagh-rust-hello`. Copy it to the ext2 data image (for example `/mnt/pagh-rust-hello`) and run inside pagh:

```text
rust /mnt/pagh-rust-hello one two
```

Programs should remain single-threaded and use ordinary file/stdio/time/allocator APIs. Unsupported Linux syscalls return `ENOSYS` and are logged once per process.
