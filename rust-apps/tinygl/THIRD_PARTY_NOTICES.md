# Third-party notices — `rust-apps/tinygl`

## TinyGL 0.4.1

| | |
|---|---|
| Компонент | TinyGL (CPU-only fixed-function OpenGL 1.1 subset) |
| Версия | **0.4.1** (файлы в архиве датированы 2022-03-05) |
| Upstream | Fabrice Bellard, <https://bellard.org/TinyGL/> |
| Артефакт | `TinyGL-0.4.1.tar.gz`, <https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz> |
| Размер | 68 800 байт |
| SHA-256 | `b314f019a778d583a3778cb14bcc2a57626a8f866cbed0693705bb1bb785b16f` |
| Лицензия | **MIT**, `Copyright (c) 1997-2022 Fabrice Bellard` |

Полный текст лицензии — [`LICENSE-TinyGL`](LICENSE-TinyGL), **байт-в-байт** из
tarball'а (включая вторую строку — одиночный пробел). В исходниках upstream
copyright-шапок нет (проверено: `grep -rl "Copyright" src/*.c src/*.h
include/GL/*.h` → 0 файлов), поэтому notice обязан жить в этом файле и в
`LICENSE-TinyGL`; распространение `c_src/` без них было бы распространением
без notice.

Смена лицензии на MIT подтверждена официальным changelog'ом upstream:

```text
## version 0.4.1:
 - changed license to MIT
```

(<https://bellard.org/TinyGL/changelog.html>; та же строка в `Changelog` внутри
tarball'а; `README` — «MIT license for easy integration in commercial designs»).

## Что именно вендорено (`c_src/`)

23 файла C-ядра (все `src/*.c`, кроме X11/NanoX/BeOS-обвязки) и нужные им
заголовки:

```text
c_src/src/api.c         c_src/src/get.c          c_src/src/select.c
c_src/src/arrays.c      c_src/src/image_util.c   c_src/src/specbuf.c
c_src/src/clear.c       c_src/src/init.c         c_src/src/texture.c
c_src/src/clip.c        c_src/src/light.c        c_src/src/vertex.c
c_src/src/error.c       c_src/src/list.c         c_src/src/zbuffer.c
c_src/src/matrix.c      c_src/src/memory.c       c_src/src/zdither.c
c_src/src/misc.c        c_src/src/msghandling.c  c_src/src/zline.c
c_src/src/zmath.c       c_src/src/ztriangle.c

c_src/src/msghandling.h  c_src/src/opinfo.h   c_src/src/specbuf.h
c_src/src/zbuffer.h      c_src/src/zfeatures.h c_src/src/zgl.h
c_src/src/zline.h        c_src/src/zmath.h    c_src/src/ztriangle.h
c_src/include/GL/gl.h
```

Изменён **ровно один** файл upstream — `c_src/src/zfeatures.h`; точный diff,
обоснование каждой строки и флаги компиляции — в [`c_src/PATCHES.md`](c_src/PATCHES.md).

## Что НЕ распространяется / не компилируется

`src/glx.c`, `src/nglx.c`, `src/oscontext.c`, `include/GL/{glx,nglx,oscontext}.h`,
каталог `BeOS/`, `examples/`, `lib/`, `Makefile`, `config.mk` — X11/NanoX/BeOS-
обвязка и сборочные файлы upstream. У Pagh нет X11/Wayland; backend — свой
PDP/1-клиент (спека `docs/tinygl-port-spec.md` §3.1, §4). Эти файлы не входят
в `c_src/` и не участвуют в сборке.

## Наши файлы

`Cargo.toml`, `build.rs`, `src/lib.rs`, `src/ffi.rs`, `src/surface.rs`,
`src/bin/tinygl-probe.rs`, `tests/*` — код Pagh-OS, MIT (корневой `LICENSE`).
Файлы, производные от TinyGL, несут шапку:

```text
// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
```