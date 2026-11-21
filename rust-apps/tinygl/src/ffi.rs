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
