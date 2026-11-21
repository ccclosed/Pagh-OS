// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Сырые `extern "C"` прототипы TinyGL 0.4.1 (MIT) — **весь** unsafe-слой крейта.
//!
//! Почему прототипы написаны руками, а не через `bindgen`:
//! * `bindgen` тянет build-зависимость и clang — ровно тот тулчейн, наличие
//!   которого PR1 и проверяет; офлайн-сборка должна оставаться простой;
//! * upstream `include/GL/gl.h` **не объявляет** immediate-mode прототипы
//!   (`glVertex3f`, `glColor3f`, `glNormal3f`, `glTexCoord*` и т. д.), хотя
//!   `src/api.c` их определяет и экспортирует. Поэтому они объявлены здесь, а
//!   upstream-заголовок не правится.
//!
//! Модуль **приватный** (`mod ffi`): безопасный слой [`crate::Context`] держит
//! инварианты (один живой `gl_ctx`, валидный буфер, согласованный state), а
//! снаружи крейта сырые вызовы недоступны.
//!
//! ## Чего в upstream 0.4.1 нет (не выдумываем)
//!
//! * `glGetError`/`GL_NO_ERROR` — **отсутствуют**: в `src/` нет ни символа, ни
//!   поля состояния. Ошибки либо невозможны, либо фатальны (`gl_fatal_error`
//!   в `error.c` → `exit(1)`). [`crate::Context::get_error`] — документированная
//!   совместимость-заглушка, а не биндинг.
//! * `glOrtho` — в `include/GL/gl.h` это пустой `inline` (`glOrtho(...) {}`),
//!   внешнего символа нет; не биндим.
//! * `glDepthFunc`/`glDepthMask` — отсутствуют; Z пишется всегда, сравнение
//!   фиксированное (`ZCMP` в `ztriangle.c`).
//! * `glDeleteLists` — **пустой `inline` в `gl.h`**, внешнего символа нет;
//!   освобождения списка в 0.4.1 нет (`delete_list` в `list.c` — `static`).
//!   Наш слой эмулирует удаление имён, но C-слот освобождается только при
//!   переопределении имени или закрытии контекста (см. `Context::delete_lists`).
//! * `glTexImage2D` принимает одно сочетание параметров (RGB/UNSIGNED_BYTE/
//!   level 0/border 0/components 3); всё остальное — `exit(1)`. Фильтры
//!   текстур (`GL_TEXTURE_MIN_FILTER`/`MAG_FILTER`) не сохраняются вообще:
//!   растеризатор всегда берёт `GL_NEAREST`, wrap всегда `GL_REPEAT`.
//!
//! ## Порядок аргументов, который легко перепутать
//!
//! `ZB_resize(zb, frame_buffer, xsize, ysize)` — **frame_buffer вторым**
//! (`zbuffer.h`), в отличие от `ZB_open`. Передавать надо тот же внешний
//! буфер, что и в `ZB_open`: при `NULL` TinyGL выделит собственный `pbuf` и
//! отвяжется от памяти вызывающего.
//!
//! Все функции — `unsafe`-контракт: указатели должны быть валидны, а контекст
//! (`gl_ctx` — глобальный указатель в `init.c`) — жив и единственен.

use std::ffi::{c_int, c_void};

/// Непрозрачный `ZBuffer` из `zbuffer.h` (структура layout-sensitive; Rust её
/// никогда не читает — только держит указатель).
#[repr(C)]
pub struct ZBuffer {
    _private: [u8; 0],
}

// --- zbuffer.c ------------------------------------------------------------

extern "C" {
    /// `ZB_open(w, h, ZB_MODE_RGBA, 0, NULL, NULL, frame_buffer)`.
    /// `frame_buffer == NULL` — TinyGL выделит буфер сам (нам не нужно: демо
    /// рендерят прямо во внешний `mmap`/`Vec`).
    pub fn ZB_open(
        xsize: c_int,
        ysize: c_int,
        mode: c_int,
        nb_colors: c_int,
        color_indexes: *mut u8,
        color_table: *mut c_int,
        frame_buffer: *mut c_void,
    ) -> *mut ZBuffer;

    /// Освобождает Z-буфер и (если TinyGL его выделял) `pbuf`; внешний
    /// `frame_buffer` не освобождается.
    pub fn ZB_close(zb: *mut ZBuffer);

    /// Переконфигурирует Z-буфер на новые размеры, **не** освобождая внешний
    /// `frame_buffer` (он передаётся вторым аргументом). `xsize` внутри
    /// маскируется `& ~3`; upstream не проверяет результат `gl_malloc` нового
    /// Z-буфера, поэтому вызывающий обязан держать размеры разумными.
    pub fn ZB_resize(zb: *mut ZBuffer, frame_buffer: *mut c_void, xsize: c_int, ysize: c_int);
}

// --- init.c ---------------------------------------------------------------

extern "C" {
    /// Создаёт глобальный `gl_ctx` поверх `zb`. Второй живой контекст в
    /// процессе — ошибка вызывающего: TinyGL не поддерживает мультиконтекст.
    pub fn glInit(zbuffer: *mut c_void);
    /// Освобождает `gl_ctx`; `ZBuffer` остаётся жив (`ZB_close` отдельно).
    pub fn glClose();
}

// --- api.c / matrix.c / get.c: состояние, матрицы, clear, примитивы --------

extern "C" {
    pub fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int);
    pub fn glClear(mask: c_int);
    pub fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    pub fn glClearDepth(depth: f64);

    pub fn glMatrixMode(mode: c_int);
    pub fn glLoadIdentity();
    pub fn glLoadMatrixf(m: *const f32);
    pub fn glMultMatrixf(m: *const f32);
    pub fn glPushMatrix();
    pub fn glPopMatrix();
    pub fn glFrustum(left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64);
    pub fn glTranslatef(x: f32, y: f32, z: f32);
    pub fn glRotatef(angle: f32, x: f32, y: f32, z: f32);
    pub fn glScalef(x: f32, y: f32, z: f32);

    /// Поддерживаемые `pname` — ровно те, что разбирает `get.c`:
    /// `GL_VIEWPORT` (4 int), `GL_MAX_{MODELVIEW,PROJECTION,TEXTURE}_STACK_DEPTH`,
    /// `GL_MAX_LIGHTS`, `GL_MAX_TEXTURE_SIZE`. На прочих upstream вызывает
    /// `gl_fatal_error("glGet: option not implemented")` → `exit(1)`, поэтому
    /// снаружи эти pname не выбираются: их задаёт только [`crate::Context`].
    pub fn glGetIntegerv(pname: c_int, params: *mut c_int);

    /// Поддерживаемые `pname`: `GL_MODELVIEW_MATRIX`/`GL_PROJECTION_MATRIX`/
    /// `GL_TEXTURE_MATRIX` (16 float, column-major) и `GL_LINE_WIDTH`/
    /// `GL_POINT_SIZE`/`*_RANGE`. На неизвестном pname upstream печатает
    /// warning и оставляет результат неопределённым; снаружи доступны только
    /// матрицы.
    pub fn glGetFloatv(pname: c_int, v: *mut f32);

    pub fn glShadeModel(mode: c_int);
    pub fn glEnable(cap: c_int);
    pub fn glDisable(cap: c_int);
    pub fn glCullFace(mode: c_int);
    pub fn glFrontFace(mode: c_int);

    // --- lighting (PR5) ---------------------------------------------------
    // Upstream 0.4.1 реализует fixed-function освещение (light.c):
    // `GL_LIGHTING` + до 16 `GL_LIGHT0..15`, материалы front/back и
    // `GL_COLOR_MATERIAL`. Эти символы экспортированы `api.c`, но в
    // `include/GL/gl.h` объявлены только `glMaterialfv`/`glLightfv` и т. п.;
    // прототипы ниже повторяют реальные сигнатуры C.

    /// `glLightfv(light, pname, params)` — 4 float для AMBIENT/DIFFUSE/
    /// SPECULAR/POSITION. `POSITION` умножается на **текущую modelview** в
    /// момент вызова (upstream `glopLight`), поэтому позиционный источник
    /// задаётся в координатах глаза, а не мира.
    pub fn glLightfv(light: c_int, pname: c_int, params: *const f32);

    /// `glMaterialfv(face, pname, params)` — 4 float; для `GL_SHININESS`
    /// upstream читает только `params[0]` (`glMaterialfv` выставляет n=1).
    pub fn glMaterialfv(face: c_int, pname: c_int, params: *const f32);

    pub fn glBegin(mode: c_int);
    pub fn glEnd();

    // Immediate-mode прототипы, которых нет в upstream `gl.h`
    // (определены в `api.c`; поведение — как в GL 1.1 fixed-function).
    pub fn glVertex2f(x: f32, y: f32);
    pub fn glVertex3f(x: f32, y: f32, z: f32);
    pub fn glVertex3fv(v: *const f32);
    pub fn glVertex4f(x: f32, y: f32, z: f32, w: f32);

    pub fn glColor3f(r: f32, g: f32, b: f32);
    pub fn glColor3fv(v: *const f32);
    pub fn glColor4f(r: f32, g: f32, b: f32, a: f32);
    pub fn glColor4fv(v: *const f32);

    pub fn glNormal3f(x: f32, y: f32, z: f32);
    pub fn glNormal3fv(v: *const f32);

    pub fn glTexCoord2f(s: f32, t: f32);
    pub fn glTexCoord2fv(v: *const f32);
    pub fn glTexCoord4f(s: f32, t: f32, r: f32, q: f32);
}

// --- texture.c / api.c: textures (PR3) ------------------------------------

extern "C" {
    /// `glGenTextures(n, textures)` — пишет `n` имён. Имя — **не объект**:
    /// `GLTexture` создаётся только в `glBindTexture` (`find_texture` →
    /// `alloc_texture`), поэтому имена, которые никто не биндил, в таблице
    /// текстур отсутствуют. Upstream берёт `max(handle) + i + 1`, то есть при
    /// `max` у `i32::MAX` переполнение `int`; наш слой такой `max` не допускает.
    pub fn glGenTextures(n: c_int, textures: *mut u32);

    /// `glDeleteTextures(n, textures)`. Для текущей текстуры upstream вызывает
    /// `glBindTexture(GL_TEXTURE_2D, 0)` (создавая объект 0) и лишь затем
    /// освобождает удаляемую; для имени `0` это оставляет `current_texture`
    /// висячим указателем (upstream-баг) — наш слой имя `0` не передаёт.
    pub fn glDeleteTextures(n: c_int, textures: *const u32);

    /// `glBindTexture(target, texture)`: `assert(target == GL_TEXTURE_2D &&
    /// texture >= 0)`; при отсутствии объекта upstream его создаёт.
    pub fn glBindTexture(target: c_int, texture: c_int);

    /// `glTexImage2D(...)`. Сочетание параметров жёстко зашито в
    /// `glopTexImage2D`: только `GL_TEXTURE_2D`, `level == 0`,
    /// `components == 3`, `border == 0`, `GL_RGB`, `GL_UNSIGNED_BYTE`; любое
    /// другое — `gl_fatal_error` → `exit(1)`. Размер, отличный от 256×256,
    /// upstream **ресемплит без интерполяции** в 256×256; mipmap-уровней нет.
    /// `pixels` читается целиком как `width * height * 3` байт с плотными
    /// строками (`glPixelStorei` поддерживает только `UNPACK_ALIGNMENT 1`).
    pub fn glTexImage2D(
        target: c_int,
        level: c_int,
        components: c_int,
        width: c_int,
        height: c_int,
        border: c_int,
        format: c_int,
        type_: c_int,
        pixels: *mut c_void,
    );

    /// `glTexEnvi`: `glopTexEnv` принимает только `GL_TEXTURE_ENV` /
    /// `GL_TEXTURE_ENV_MODE` / `GL_DECAL`, иначе `exit(1)`.
    pub fn glTexEnvi(target: c_int, pname: c_int, param: c_int);

    /// `glTexParameteri`: `glopTexParameter` принимает только
    /// `GL_TEXTURE_2D` и `GL_TEXTURE_WRAP_S`/`GL_TEXTURE_WRAP_T` со значением
    /// `GL_REPEAT`; фильтры (`GL_TEXTURE_MIN_FILTER`/`GL_TEXTURE_MAG_FILTER`)
    /// **молча игнорируются** — растеризатор всегда берёт один тексель
    /// (`GL_NEAREST`), и wrap всегда повторяет (маска координат на 256).
    pub fn glTexParameteri(target: c_int, pname: c_int, param: c_int);
}

// --- list.c: display lists (PR3) ------------------------------------------

extern "C" {
    /// `glGenLists(range)` возвращает первое имя диапазона **или 0**. Внимание:
    /// upstream возвращает `0` и при успехе, если диапазон начинается с нуля
    /// (`list = i - range + 1`), поэтому отличить успех от отказа по
    /// возвращённому значению нельзя — наш слой считает занятость сам (см.
    /// `lists.rs`) и сверяет предсказанное имя с возвращённым.
    pub fn glGenLists(range: c_int) -> u32;

    /// `glNewList(list, mode)`: `assert(mode == GL_COMPILE ||
    /// mode == GL_COMPILE_AND_EXECUTE)` и `assert(compile_flag == 0)`;
    /// список записывает все последующие операции через `gl_add_op`.
    pub fn glNewList(list: u32, mode: c_int);

    /// `glEndList`: `assert(compile_flag == 1)`.
    pub fn glEndList();

    /// `glCallList(list)`: на неизвестном имени — `gl_fatal_error` → `exit(1)`;
    /// на списке, вызов которого уходит в рекурсию, — переполнение C-стека.
    pub fn glCallList(list: u32);

    /// `glIsList(list)`: 1, если слот занят (`glGenLists`/`glNewList` создают
    /// слот сразу, даже пустой). В upstream `glDeleteLists` — **пустой
    /// `inline` в `gl.h`**, внешнего символа нет; освобождения списка в 0.4.1
    /// не существует вообще (`delete_list` в `list.c` — `static`).
    pub fn glIsList(list: u32) -> c_int;
}

/// `zbuffer.h`: `ZB_MODE_RGBA` (32 bit rgba mode). Внутренний формат задан
/// `TGL_FEATURE_RENDER_BITS 32` (см. `c_src/PATCHES.md`), внешний — `PIXEL` =
/// `u32 0x00RRGGBB` (little-endian) = XRGB8888 композитора.
pub const ZB_MODE_RGBA: c_int = 3;

/// `GL_COLOR_BUFFER_BIT` (`gl.h`).
pub const GL_COLOR_BUFFER_BIT: c_int = 0x0000_4000;
/// `GL_DEPTH_BUFFER_BIT` (`gl.h`).
pub const GL_DEPTH_BUFFER_BIT: c_int = 0x0000_0100;

/// `GL_MODELVIEW`.
pub const GL_MODELVIEW: c_int = 0x1700;
/// `GL_PROJECTION`.
pub const GL_PROJECTION: c_int = 0x1701;

/// `GL_VIEWPORT` — `get.c` пишет 4 int.
pub const GL_VIEWPORT: c_int = 0x0BA2;
/// `GL_MODELVIEW_MATRIX` — 16 float, column-major.
pub const GL_MODELVIEW_MATRIX: c_int = 0x0BA6;
/// `GL_PROJECTION_MATRIX` — 16 float, column-major.
pub const GL_PROJECTION_MATRIX: c_int = 0x0BA7;

/// `GL_MAX_MODELVIEW_STACK_DEPTH` (upstream `zgl.h`: 32).
pub const GL_MAX_MODELVIEW_STACK_DEPTH: c_int = 0x0D36;
/// `GL_MAX_PROJECTION_STACK_DEPTH` (upstream `zgl.h`: 8).
pub const GL_MAX_PROJECTION_STACK_DEPTH: c_int = 0x0D38;
/// `GL_MAX_LIGHTS` (upstream `zgl.h`: 16).
pub const GL_MAX_LIGHTS: c_int = 0x0D31;
/// `GL_MAX_TEXTURE_SIZE` (`get.c` возвращает 256).
pub const GL_MAX_TEXTURE_SIZE: c_int = 0x0D33;

/// `GL_LIGHTING` — включает расчёт освещения в `vertex.c`.
pub const GL_LIGHTING: c_int = 0x0B50;
/// `GL_NORMALIZE` — нормализует нормали после modelview (upstream поддерживает).
pub const GL_NORMALIZE: c_int = 0x0BA1;
/// `GL_LIGHT0` — первый источник; валидны `GL_LIGHT0..GL_LIGHT0+15`.
pub const GL_LIGHT0: c_int = 0x4000;
/// `GL_AMBIENT` (и материал, и источник).
pub const GL_AMBIENT: c_int = 0x1200;
/// `GL_DIFFUSE` (и материал, и источник).
pub const GL_DIFFUSE: c_int = 0x1201;
/// `GL_SPECULAR` (и материал, и источник).
pub const GL_SPECULAR: c_int = 0x1202;
/// `GL_POSITION` — параметр источника (x,y,z,w; w=0 — направленный).
pub const GL_POSITION: c_int = 0x1203;
/// `GL_EMISSION` — параметр материала.
pub const GL_EMISSION: c_int = 0x1600;
/// `GL_SHININESS` — параметр материала; upstream ждёт одно значение.
pub const GL_SHININESS: c_int = 0x1601;
/// `GL_AMBIENT_AND_DIFFUSE` — материал: ambient и diffuse одним вызовом.
pub const GL_AMBIENT_AND_DIFFUSE: c_int = 0x1602;

/// `GL_CULL_FACE` — единственный cull-переключатель upstream.
pub const GL_CULL_FACE: c_int = 0x0B44;
/// `GL_DEPTH_TEST` — для линий; для заливки Z-тест идёт всегда.
pub const GL_DEPTH_TEST: c_int = 0x0B71;
/// `GL_FRONT`.
pub const GL_FRONT: c_int = 0x0404;
/// `GL_BACK`.
pub const GL_BACK: c_int = 0x0405;
/// `GL_FRONT_AND_BACK`.
pub const GL_FRONT_AND_BACK: c_int = 0x0408;
/// `GL_CCW` — front face по умолчанию.
pub const GL_CCW: c_int = 0x0901;
/// `GL_CW`.
pub const GL_CW: c_int = 0x0900;

/// `GL_TEXTURE` — третий режим матричного стека (`glMatrixMode`).
pub const GL_TEXTURE: c_int = 0x1702;
/// `GL_TEXTURE_MATRIX` — 16 float, column-major.
pub const GL_TEXTURE_MATRIX: c_int = 0x0BA8;
/// `GL_MAX_TEXTURE_STACK_DEPTH` (upstream `zgl.h`: 8).
pub const GL_MAX_TEXTURE_STACK_DEPTH: c_int = 0x0D39;

/// `GL_TEXTURE_2D` — единственная текстурная цель upstream.
pub const GL_TEXTURE_2D: c_int = 0x0DE1;
/// `GL_RGB` — единственный внешний формат `glTexImage2D`.
pub const GL_RGB: c_int = 0x1907;
/// `GL_UNSIGNED_BYTE` — единственный тип пикселей `glTexImage2D`.
pub const GL_UNSIGNED_BYTE: c_int = 0x1401;
/// `GL_TEXTURE_ENV` — target `glTexEnvi`.
pub const GL_TEXTURE_ENV: c_int = 0x2300;
/// `GL_TEXTURE_ENV_MODE` — единственный pname `glTexEnvi`.
pub const GL_TEXTURE_ENV_MODE: c_int = 0x2200;
/// `GL_DECAL` — единственный режим `glTexEnvi` (и единственное, что умеет
/// 32-битный растеризатор: тексель заменяет цвет вершины).
pub const GL_DECAL: c_int = 0x2101;
/// `GL_TEXTURE_WRAP_S`.
pub const GL_TEXTURE_WRAP_S: c_int = 0x2802;
/// `GL_TEXTURE_WRAP_T`.
pub const GL_TEXTURE_WRAP_T: c_int = 0x2803;
/// `GL_TEXTURE_MIN_FILTER` — upstream молча игнорирует (всегда NEAREST).
pub const GL_TEXTURE_MIN_FILTER: c_int = 0x2801;
/// `GL_TEXTURE_MAG_FILTER` — upstream молча игнорирует (всегда NEAREST).
pub const GL_TEXTURE_MAG_FILTER: c_int = 0x2800;
/// `GL_NEAREST` — фильтр, который фактически всегда используется.
pub const GL_NEAREST: c_int = 0x2600;
/// `GL_REPEAT` — единственный допустимый wrap.
pub const GL_REPEAT: c_int = 0x2901;

/// `GL_COMPILE` — режим `glNewList`: только запись, без исполнения.
pub const GL_COMPILE: c_int = 0x1300;
/// `GL_COMPILE_AND_EXECUTE` — запись и немедленное исполнение.
pub const GL_COMPILE_AND_EXECUTE: c_int = 0x1301;

/// `zgl.h: MAX_DISPLAY_LISTS` — размер массива слотов списков. Upstream
/// индексирует его **без проверки границ**, поэтому имена `>= 1024` не должны
/// доходить до C: наш слой допускает только имена, выданные `glGenLists`.
pub const MAX_DISPLAY_LISTS: u32 = 1024;
