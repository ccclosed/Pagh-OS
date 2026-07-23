# Pipes and poll in pagh

Implemented Linux x86_64 syscalls: `poll(7)`, `pipe(22)`, `pipe2(293)`, `select(23)`,
`pselect6(270)` and `ppoll(271)`.

- 64 KiB in-kernel byte queue.
- Blocking endpoints cooperatively yield instead of holding a spinlock.
- `O_NONBLOCK` returns `EAGAIN` for empty/full queues.
- Closing the final writer produces EOF and `POLLHUP` on readers.
- Closing the final reader produces `EPIPE` and `POLLERR` on writers.
- `O_CLOEXEC` is accepted (also via the `FIOCLEX`/`FIONCLEX` ioctls); descriptor
  closing will become active with `execve`.
- `select`/`pselect6` report the cooked-tty stdin as readable and pipes by real queue
  state; timeouts are polled with cooperative yields (this is what unblocks GNU
  readline in the CPython REPL).

Build the smoke fixture on the Fedora host with the musl cross compiler:

```bash
x86_64-linux-musl-gcc -static -O2 tests/fixtures/pipe_poll.c -o pipe-poll
```

Copy `pipe-poll` to the ext2 image and run it in pagh. Exit status `0` means the pipe, readiness, data and HUP checks passed.
