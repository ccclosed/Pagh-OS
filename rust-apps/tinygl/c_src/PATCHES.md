# Патч upstream TinyGL 0.4.1

Источник — tarball `TinyGL-0.4.1.tar.gz` (sha256
`b314f019a778d583a3778cb14bcc2a57626a8f866cbed0693705bb1bb785b16f`, 68 800 байт,
файлы 2022-03-05), <https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz>.

**Правится ровно один файл upstream — `src/zfeatures.h`.** Всё остальное в
`c_src/` — байт-в-байт из tarball'а. Это упрощает сверку и обновление:
`diff -r` против распакованного tarball'а должен показывать только этот файл.

## diff

```diff
--- TinyGL-0.4.1/src/zfeatures.h
+++ tinygl/c_src/src/zfeatures.h
@@ -29,15 +29,16 @@
 
 /* enable various convertion code from internal pixel format (usually
    16 bits per pixel) to any external format */
-#define TGL_FEATURE_16_BITS        1
-#define TGL_FEATURE_8_BITS         1
-#define TGL_FEATURE_24_BITS        1
+#define TGL_FEATURE_16_BITS       0
+/* 8-bit dithering requires RENDER_BITS==16; disabled (checked with defined(), not value) */
+// #define TGL_FEATURE_8_BITS      1
+#define TGL_FEATURE_24_BITS       0
 #define TGL_FEATURE_32_BITS        1
 
 
 //#define TGL_FEATURE_RENDER_BITS    15
-#define TGL_FEATURE_RENDER_BITS    16
+// #define TGL_FEATURE_RENDER_BITS    16
 //#define TGL_FEATURE_RENDER_BITS    24
-//#define TGL_FEATURE_RENDER_BITS    32
+#define TGL_FEATURE_RENDER_BITS    32
 
 #endif /* _tgl_features_h_ */
```

## Почему каждая строка

| Строка | Почему |
|---|---|
| `TGL_FEATURE_RENDER_BITS 32` | Нужен `PIXEL = u32 0x00RRGGBB` (little-endian) — ровно `XRGB8888` композитора Pagh. При дефолтном `16` пиксель `u16`, и рендер во внешний `u32`-буфер не совпадает по формату. |
| `TGL_FEATURE_8_BITS` **закомментирован, не `0`** | И `zdither.c`, и `zbuffer.c` проверяют `#if defined(TGL_FEATURE_8_BITS)` / `#ifdef`, а не значение. `0` оставляет `defined()` истинным: `zdither.c` требует `RENDER_BITS == 16` и даёт `#error`, а `zbuffer.c` в `ZB_open`/`ZB_close` ссылается на `ZB_initDither`/`ZB_closeDither`, которых при `RENDER_BITS 32` нет → undefined reference. Комментарий обязателен. |
| `TGL_FEATURE_16_BITS 0`, `TGL_FEATURE_24_BITS 0` | Внешние 16/24-битные конверсии не нужны: единственный внешний формат — `ZB_MODE_RGBA` (32bpp). Спека §5.3 помечает их как опциональные; выключены, чтобы не тянуть мёртвый код. |
| `TGL_FEATURE_32_BITS 1` | Оставлен как есть: `ZB_MODE_RGBA` — 32bpp-путь. |

## Что НЕ вендорится (и почему)

Не копируются и не компилируются: `src/glx.c`, `src/nglx.c`,
`src/oscontext.c`, `include/GL/{glx,nglx,oscontext}.h`, каталог `BeOS/`,
`examples/`, `lib/`, `Makefile`, `config.mk`. Это X11/NanoX/BeOS-обвязка и
сборочные файлы upstream; у Pagh нет X11/Wayland, а backend — свой `pagh-gl-demo`
поверх PDP/1 (спека §3.1, §4).

## Флаги компиляции (в `build.rs`, не патч)

```text
-O2 -fno-strict-aliasing -Wno-implicit-fallthrough
-I c_src/include -I c_src/src
```

* `-fno-strict-aliasing` — как в эталонной пробе спеки; upstream-код не
  рассчитан на strict-aliasing оптимизации (union/`PIXEL`-касты).
* `-Wno-implicit-fallthrough` — три предупреждения в `get.c` (`get.c:42,44,67`),
  upstream их не глушит; без флага сборка шумная, но не падает.
* `-lm` — только для glibc-хоста (`build.rs`): на musl математика уже в libc.a,
  а `-lm` в self-contained musl-таргете рискует не найти файл.

## Проверка после обновления/переноса

```sh
cd rust-apps/tinygl
cargo test                              # criterion6: digest 0x3f7e5ece33386f9b
cargo run --bin tinygl-probe            # печатает digest/center/corner + interp
cargo build --target x86_64-unknown-linux-musl   # нужен musl C-компилятор
```

Если сверять с распакованным tarball'ом вручную:

```sh
diff -ru /tmp/TinyGL-0.4.1/src rust-apps/tinygl/c_src/src
# ожидаемо: только zfeatures.h
diff -u  /tmp/TinyGL-0.4.1/include/GL/gl.h rust-apps/tinygl/c_src/include/GL/gl.h
# пусто
```