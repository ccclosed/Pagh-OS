// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Сырые `extern "C"` прототипы TinyGL 0.4.1 (MIT).
//!
//! Почему прототипы написаны руками, а не через `bindgen`:
//! * `bindgen` тянет build-зависимость и clang — ровно тот тулчейн, наличие
//!   которого PR1 и проверяет; офлайн-сборка должна оставаться простой;
//! * upstream `include/GL/gl.h` **не объявляет** immediate-mode прототипы
//!   (`glVertex3f`, `glColor3f`, `glNormal3f`, `glTexCoord*` и т. д.), хотя
//!   `src/api.c` их определяет и экспортирует. Поэтому они объявлены здесь, а
//!   upstream-заголовок не правится.
//!
//! Все функции — `unsafe`-контракт: указатели должны быть валидны, а контекст
//! (`gl_ctx` — глобальный указатель в `init.c`) — жив и единственен. Безопасная
//! обёртка (`crate::Context`) держит эти инварианты; напрямую этот модуль
//! пользователю не нужен, но `pub` — для PR2+ (массивы, текстуры) и для
//! честной документации ABI.
//!
//! Контекст: `glInit(zb)` создаёт глобальный `gl_ctx`; `glClose()` освобождает
//! контекст, **но не** `ZBuffer`. Поэтому обёртка всегда делает
//! `glClose()` → `ZB_close()` именно в этом порядке.

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
    /// `frame_buffer == NULL` — TinyGL выделит буфер сам (нам не нужно: PR1/демо
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
}

// --- init.c ---------------------------------------------------------------

extern "C" {
    /// Создаёт глобальный `gl_ctx` поверх `zb`. Второй живой контекст в
    /// процессе — ошибка вызывающего: TinyGL не поддерживает мультиконтекст.
    pub fn glInit(zbuffer: *mut c_void);
    /// Освобождает `gl_ctx`; `ZBuffer` остаётся жив (`ZB_close` отдельно).
    pub fn glClose();
}

// --- api.c / matrix.c: состояние, матрицы, clear, примитивы --------------

extern "C" {
    pub fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int);
    pub fn glClear(mask: c_int);
    pub fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    pub fn glClearDepth(depth: f64);

    pub fn glMatrixMode(mode: c_int);
    pub fn glLoadIdentity();
    pub fn glFrustum(left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64);
    pub fn glTranslatef(x: f32, y: f32, z: f32);
    pub fn glRotatef(angle: f32, x: f32, y: f32, z: f32);
    pub fn glScalef(x: f32, y: f32, z: f32);

    pub fn glShadeModel(mode: c_int);
    pub fn glEnable(cap: c_int);
    pub fn glDisable(cap: c_int);

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

/// `GL_TRIANGLES`.
pub const GL_TRIANGLES: c_int = 0x0004;
