# Pipes and poll in pagh

Implemented Linux x86_64 syscalls: `poll(7)`, `pipe(22)` and `pipe2(293)`.

- 64 KiB in-kernel byte queue.
- Blocking endpoints cooperatively yield instead of holding a spinlock.
- `O_NONBLOCK` returns `EAGAIN` for empty/full queues.
- Closing the final writer produces EOF and `POLLHUP` on readers.
- Closing the final reader produces `EPIPE` and `POLLERR` on writers.
- `O_CLOEXEC` is accepted; descriptor closing will become active with `execve`.

Build the smoke fixture on the Fedora host with the musl cross compiler:

```bash
x86_64-linux-musl-gcc -static -O2 tests/fixtures/pipe_poll.c -o pipe-poll
```

Copy `pipe-poll` to the ext2 image and run it in pagh. Exit status `0` means the pipe, readiness, data and HUP checks passed.
