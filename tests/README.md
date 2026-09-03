# `tests/` — on-target syscall-фикстуры (C)

In-guest фикстуры проверки syscall-совместимости, не автоматизированные хост-тесты.
Собираются вручную musl-кросс-компилятором, копируются в ext2-образ, запускаются под pagh.

## Файлы

| Файл | Что проверяет |
|---|---|
| `fixtures/pipe_poll.c` | `pipe` + `poll`: POLLIN до закрытия писателя, POLLHUP после, read-roundtrip. Различные exit-коды 10–15 = конкретные отказы |
| `fixtures/clone-thread.c` | `clone(CLONE_VM\|CLONE_FS\|CLONE_FILES\|CLONE_SIGHAND\|CLONE_THREAD\|CLONE_SYSVSEM\|CLONE_CHILD_SETTID\|CLONE_CHILD_CLEARTID)` — тред + `futex` WAIT на child TID |

## Сборка и запуск

```sh
x86_64-linux-musl-gcc -static -O2 tests/fixtures/pipe_poll.c -o pipe-poll
```

(нужен `musl-tools`). Бинарь кладётся в ext2-образ и запускается: `lxrun /mnt/pipe-poll`.
Код возврата — результат теста. Makefile-таргета, который собирает фикстуры, нет —
см. `PIPE-POLL.md` в корне.

## Грабли

- Бинари обязаны быть **static** (musl) — динамические пойдут, только если в образе
  есть glibc-окружение (provisioning/python3).
