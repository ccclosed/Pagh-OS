# Upstream: как обновлять / проверять `rust-apps/tinygl`

TinyGL **0.4.1** (2022-03-05) — последний upstream-релиз; проект заморожен с
2022. Обновление возможно только если Bellard выпустит новую версию. Версии
0.1–0.4 **не берём**: у них другая лицензия и другой код (см.
`THIRD_PARTY_NOTICES.md`, `docs/tinygl-port-spec.md` §2).

## 1. Скачать и проверить артефакт

```sh
curl -sSLO https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz
sha256sum TinyGL-0.4.1.tar.gz
# ОБЯЗАТЕЛЬНО: b314f019a778d583a3778cb14bcc2a57626a8f866cbed0693705bb1bb785b16f
stat -c %s TinyGL-0.4.1.tar.gz
# ОБЯЗАТЕЛЬНО: 68800
tar xzf TinyGL-0.4.1.tar.gz
```

Если sha256 не совпал — **стоп**: это не тот артефакт, дальше не идти.

## 2. Обновить `c_src/` (только нужное)

```sh
T=rust-apps/tinygl/c_src
rm -f "$T"/src/*.c "$T"/src/*.h
for f in api arrays clear clip error get image_util init light list matrix \
         memory misc msghandling select specbuf texture vertex zbuffer \
         zdither zline zmath ztriangle; do
  cp "TinyGL-0.4.1/src/$f.c" "$T/src/"
done
cp TinyGL-0.4.1/src/*.h "$T/src/"
cp TinyGL-0.4.1/include/GL/gl.h "$T/include/GL/"
cp TinyGL-0.4.1/LICENSE LICENSE-TinyGL   # байт-в-байт, не пересохранять
```

`glx.c`/`nglx.c`/`oscontext.c`, `include/GL/{glx,nglx,oscontext}.h`, `BeOS/`,
`examples/`, `lib/`, `Makefile`, `config.mk` — **не копируются** (X11/NanoX/
BeOS-обвязка и upstream-сборка; см. `PATCHES.md`).

## 3. Применить минимальный патч

Единственная правка upstream — `src/zfeatures.h`: `TGL_FEATURE_RENDER_BITS 32`,
`TGL_FEATURE_8_BITS` **закомментировать (не в `0`!)**, внешние 16/24 → `0`.
Точный diff и обоснование — [`c_src/PATCHES.md`](c_src/PATCHES.md). Проверка:

```sh
diff -ru TinyGL-0.4.1/src rust-apps/tinygl/c_src/src
# ожидаемо: только zfeatures.h
```

## 4. Сверить лицензию

```sh
cmp TinyGL-0.4.1/LICENSE rust-apps/tinygl/LICENSE-TinyGL && echo IDENTICAL
```

Вторая строка upstream `LICENSE` — **одиночный пробел** (не пустая строка);
`cmp` должен пройти без вывода. Не набирайте файл руками.

## 5. Прогнать probe (criterion 6 — строгий)

Эталон спеки (`docs/tinygl-port-spec.md` §5.2, критерий 6) — детерминированный
треугольник в `fb[64*64]`:

```text
digest=0x3f7e5ece33386f9b center=0x003c4c7a corner=0x00000000
```

```sh
cd rust-apps/tinygl
cargo test                              # host: criterion6 + surface + probe binary
cargo run --bin tinygl-probe            # печатает digest/center/corner + interp/elf
```

Для musl нужен C-компилятор для musl (`build.rs` детектит
`CC_<target>` → `<target>-gcc` → `musl-gcc` → `clang --target`):

```sh
sudo apt install musl-tools             # Debian/Ubuntu; Fedora: dnf install musl-gcc
cargo build --release --target x86_64-unknown-linux-musl
./target/x86_64-unknown-linux-musl/release/tinygl-probe
readelf -l target/x86_64-unknown-linux-musl/release/tinygl-probe | grep INTERP
# ожидаемо: пусто (static-pie); `tinygl-probe` сам печатает interp=none
```

## 6. Зафиксированные дайджесты PR1 (оба)

| Сборка | Компилятор C | digest | center | corner | Вердикт |
|---|---|---|---|---|---|
| host `x86_64-unknown-linux-gnu`, `-O2` | gcc 16.2.1 (Red Hat 16.2.1-2) | `0x3f7e5ece33386f9b` | `0x003c4c7a` | `0x00000000` | **PASS** |
| `x86_64-unknown-linux-musl`, release | `musl-gcc` 12.2.0 (Debian 12.2.0-14+deb12u1) | `0x3f7e5ece33386f9b` | `0x003c4c7a` | `0x00000000` | **PASS** |

Оба дайджеста **совпали байт-в-байт** — FP-reassociation между host-gcc и
musl-gcc не наблюдается, классифицировать нечего. musl-бинарь: `PT_INTERP`
отсутствует, `elf=static-pie`; на host-машине запускается и печатает тот же
эталон.

Если будущая сборка (другой gcc/clang/оптимизация) даст другой дайджест —
**не «принимать и записывать»**: спека §5.2 разрешает только доказанную
FP-reassociation при идентичной геометрии (тот же `center`, та же закрашенная
площадь/цвета, меняются лишь младшие биты интерполяции), и оба дайджеста
фиксируются здесь. Другой `center`/площадь/цвет = **FAIL**, PR1 блокируется до
решения lead'а.

## 7. Что не патчим (осознанно)

* `gl_fatal_error` (`src/error.c`) вызывает `exit(1)` — upstream-поведение,
  в v1 не трогаем; это названное ограничение библиотеки.
* depth/маски — строго upstream: `glDepthMask`/`glDepthFunc` нет, Z пишется
  всегда (кроме линий depth test не влияет), `clear.c` заливает Z нулём и
  игнорирует `glClearDepth`.
* `-ffast-math` и любые FP-флаги, меняющие результаты, не включаются: они
  сломали бы criterion 6.