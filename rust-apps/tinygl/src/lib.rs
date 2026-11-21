// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! # `tinygl` — тонкая безопасная обёртка над vendored TinyGL 0.4.1
//!
//! **PR1 build-probe.** Это не «порт OpenGL» и не GPU: upstream C 0.4.1
//! (MIT, Fabrice Bellard) компилируется как есть (один патч `zfeatures.h`,
//! см. `c_src/PATCHES.md`), а этот крейт даёт ровно тот FFI, который нужен
//! build-probe и будущим PR2+: контекст, viewport, clear, матрицы,
//! immediate-mode треугольники. Никаких shaders/GLSL/FBO/VBO/EGL/GLX/Wayland/
//! Mesa и никакой аппаратной растеризации — только CPU.
//!
//! ## Честные ограничения (унаследованы от upstream 0.4.1)
//!
//! * fixed-function subset OpenGL 1.1; **нет** `glDrawArrays`/`glDrawElements`
//!   (в v1 клиентские массивы не используются и не обещаются);
//! * Z-буфер **16-битный** (`ZB_Z_BITS 16`), фиксированная точка;
//! * `stride` обязан быть `width * 4` (TinyGL не выражает паддинг), ширина
//!   обязана быть кратна 4 (`ZB_resize` делает `xsize &= ~3`);
//! * `glClear` чистит цвет и Z целиком (не по scissor), а Z всегда заливается
//!   нулём: `glClearDepth`/`z` в `clear()` upstream игнорирует (TODO в
//!   `clear.c`), так что не полагайтесь на начальный Z;
//! * depth test реально влияет только на линии; depth-mask/`glDepthFunc` нет —
//!   строго upstream, без форка (решение lead, спека §3.1);
//! * **один `Context` на процесс**: `gl_ctx` в TinyGL — глобальный указатель
//!   (`init.c`), поэтому `Context` не `Send`/`Sync`, а второй живой контекст
//!   возвращает [`FbError::ContextAlreadyLive`];
//! * внутренний fatal TinyGL (`gl_fatal_error` в `error.c`) вызывает `exit(1)`
//!   с сообщением в stderr — в v1 не патчим (upstream behaviour).
//!
//! Формат пикселя в 32-битном режиме — `PIXEL = u32 0x00RRGGBB` little-endian,
//! то есть `XRGB8888` композитора Pagh (`rust-apps/pagh-desktop/src/surface.rs`).
//! Рендер идёт **прямо во внешний буфер** вызывающего (`ZB_open(..., fb)`),
//! без копии и без конверсии.
//!
//! ## Сборка
//!
//! ```text
//! cd rust-apps/tinygl
//! cargo test                                   # host (gnu), C-компилятор = cc/gcc
//! cargo run --bin tinygl-probe                 # печатает digest/center/corner
//! cargo build --target x86_64-unknown-linux-musl
//! ```
//!
//! Второй командой нужен **musl C-компилятор**; `build.rs` ищет
//! `CC_x86_64_unknown_linux_musl` → `x86_64-unknown-linux-musl-gcc` →
//! `musl-gcc` → `clang --target=...` и падает с инструкцией (`apt install
//! musl-tools`), если ничего нет. Это detection-часть PR1 и одновременно
//! честный ответ на вопрос «заводится ли C-тулчейн».

pub mod ffi;
mod surface;

pub use surface::{FbError, Surface, MAX_SURFACE_DIM};

use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

/// Дайджест пикселей, которым PR1 фиксирует upstream-поведение.
///
/// FNV-1a 64 по little-endian байтам каждого `u32`-пикселя, **с offset basis
/// эталонной пробы** (`1469598103934665603`, а не стандартным
/// `14695981039346656037`): именно так посчитан эталон спеки
/// `0x3f7e5ece33386f9b`. Prime — стандартный `1099511628211`.
///
/// Это намеренно совместимо с PR1-пробой и `render_goldens.rs`; менять
/// константы нельзя — изменится эталон.
pub fn pixel_digest(pixels: &[u32]) -> u64 {
    let mut hash: u64 = 1_469_598_103_934_665_603;
    for pixel in pixels {
        for byte in pixel.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(1_099_511_628_211);
        }
    }
    hash
}

/// Режим матричного стека (`glMatrixMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixMode {
    ModelView,
    Projection,
}

impl MatrixMode {
    fn to_gl(self) -> i32 {
        match self {
            MatrixMode::ModelView => ffi::GL_MODELVIEW,
            MatrixMode::Projection => ffi::GL_PROJECTION,
        }
    }
}

/// Примитив для `glBegin`/`glEnd` (GL 1.1 fixed-function subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    Points,
    Lines,
    LineLoop,
    LineStrip,
    Triangles,
    TriangleStrip,
    TriangleFan,
    Quads,
    QuadStrip,
    Polygon,
}

impl Primitive {
    fn to_gl(self) -> i32 {
        match self {
            Primitive::Points => 0x0000,
            Primitive::Lines => 0x0001,
            Primitive::LineLoop => 0x0002,
            Primitive::LineStrip => 0x0003,
            Primitive::Triangles => 0x0004,
            Primitive::TriangleStrip => 0x0005,
            Primitive::TriangleFan => 0x0006,
            Primitive::Quads => 0x0007,
            Primitive::QuadStrip => 0x0008,
            Primitive::Polygon => 0x0009,
        }
    }
}

/// Глобальный маркер «в процессе живёт контекст». TinyGL хранит `gl_ctx`
/// глобально (`init.c`), поэтому два контекста одновременно невозможны;
/// маркер делает это явной ошибкой вместо тихой порчи чужого состояния.
static LIVE_CONTEXT: AtomicBool = AtomicBool::new(false);

struct LiveGuard;

impl LiveGuard {
    fn acquire() -> Result<Self, FbError> {
        LIVE_CONTEXT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| LiveGuard)
            .map_err(|_| FbError::ContextAlreadyLive)
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        LIVE_CONTEXT.store(false, Ordering::Release);
    }
}

/// TinyGL-контекст поверх [`Surface`].
///
/// `Drop` делает `glClose()` → `ZB_close()` (именно в этом порядке:
/// `glClose` освобождает `gl_ctx`, но не `ZBuffer`). `Context` держит
/// `&mut Surface`, поэтому буфер не может быть перевыделен, пока контекст жив.
///
/// **Не `Send`/`Sync`**: `gl_ctx` глобальный, вызовы GL должны идти из одного
/// потока (и это единственный живой контекст процесса).
pub struct Context<'a> {
    zb: NonNull<ffi::ZBuffer>,
    surface: Surface<'a>,
    _live: LiveGuard,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'a> Context<'a> {
    /// Открыть `ZBuffer` поверх плотного буфера и создать `glInit`-контекст.
    ///
    /// Буфер уже провалидирован [`Surface::new`]; здесь остаётся только
    /// `ZB_open` и `glInit`.
    pub fn new(mut surface: Surface<'a>) -> Result<Self, FbError> {
        let live = LiveGuard::acquire()?;
        let width = surface.width() as i32;
        let height = surface.height() as i32;
        // SAFETY: `surface.as_mut_ptr()` валиден для width*height u32 на всё
        // время жизни `surface` (который переезжает в Context), эксклюзивный
        // доступ гарантирован &mut; NULL для nb_colors/color_indexes/
        // color_table — upstream не читает их в режиме ZB_MODE_RGBA.
        let zb = unsafe {
            ffi::ZB_open(
                width,
                height,
                ffi::ZB_MODE_RGBA,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                surface.as_mut_ptr().cast(),
            )
        };
        let Some(zb) = NonNull::new(zb) else {
            return Err(FbError::OpenFailed);
        };
        // SAFETY: `zb` только что создан ZB_open и ещё не передан никому;
        // LiveGuard гарантирует, что gl_ctx не занят другим контекстом.
        unsafe { ffi::glInit(zb.as_ptr().cast()) };
        Ok(Context {
            zb,
            surface,
            _live: live,
            _not_send_sync: PhantomData,
        })
    }

    /// Явно закрыть контекст (то же, что `Drop`); `by value` исключает двойной
    /// `finish`/`ZB_close`.
    pub fn finish(self) {
        drop(self);
    }

    pub fn surface(&self) -> &Surface<'a> {
        &self.surface
    }

    /// Прочитать пиксели, отрендеренные C-кодом (формат `0x00RRGGBB`).
    pub fn pixels(&self) -> &[u32] {
        self.surface.pixels()
    }

    /// Изменяемые пиксели — например, чтобы очистить буфер до кадра.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.surface.pixels_mut()
    }

    pub fn set_viewport(&self, x: i32, y: i32, width: u32, height: u32) {
        // GL принимает int; размеры поверхности ограничены MAX_SURFACE_DIM,
        // поэтому насыщение здесь — защита от случайного u32 > i32::MAX.
        let w = i32::try_from(width).unwrap_or(i32::MAX);
        let h = i32::try_from(height).unwrap_or(i32::MAX);
        // SAFETY: единственный живой gl_ctx создан в new(); значения — int.
        unsafe { ffi::glViewport(x, y, w, h) };
    }

    /// `glClearColor` (float, как в GL). Альфа TinyGL в 32bpp-очистке не
    /// используется, но хранится в состоянии.
    pub fn clear_color(&self, r: f32, g: f32, b: f32, a: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glClearColor(r, g, b, a) };
    }

    /// `glClearDepth` (double, как в GL). Upstream `glopClear` игнорирует это
    /// значение и заливает Z нулём (`clear.c`, TODO) — не полагайтесь на него.
    pub fn clear_depth(&self, depth: f64) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glClearDepth(depth) };
    }

    /// Очистить цвет (и Z) заданным `0x00RRGGBB`; `z` принимается для
    /// совместимости с GL, но upstream 0.4.1 его игнорирует.
    pub fn clear(&self, color: u32, z: u16) {
        let r = ((color >> 16) & 0xff) as f32 / 255.0;
        let g = ((color >> 8) & 0xff) as f32 / 255.0;
        let b = (color & 0xff) as f32 / 255.0;
        self.clear_color(r, g, b, 1.0);
        self.clear_depth(f64::from(z) / 65535.0);
        // SAFETY: единственный живой gl_ctx создан в new(); маска — константы
        // gl.h, которые понимает upstream glClear.
        unsafe { ffi::glClear(ffi::GL_COLOR_BUFFER_BIT | ffi::GL_DEPTH_BUFFER_BIT) };
    }

    pub fn matrix_mode(&self, mode: MatrixMode) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glMatrixMode(mode.to_gl()) };
    }

    pub fn load_identity(&self) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glLoadIdentity() };
    }

    pub fn frustum(&self, left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glFrustum(left, right, bottom, top, near, far) };
    }

    pub fn translate(&self, x: f32, y: f32, z: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glTranslatef(x, y, z) };
    }

    pub fn rotate(&self, angle: f32, x: f32, y: f32, z: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glRotatef(angle, x, y, z) };
    }

    pub fn scale(&self, x: f32, y: f32, z: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glScalef(x, y, z) };
    }

    pub fn begin(&self, primitive: Primitive) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glBegin(primitive.to_gl()) };
    }

    pub fn end(&self) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glEnd() };
    }

    pub fn vertex2f(&self, x: f32, y: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glVertex2f(x, y) };
    }

    pub fn vertex3f(&self, x: f32, y: f32, z: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glVertex3f(x, y, z) };
    }

    pub fn vertex3fv(&self, v: &[f32; 3]) {
        // SAFETY: v — валидная ссылка на 3 float; glVertex3fv их копирует.
        unsafe { ffi::glVertex3fv(v.as_ptr()) };
    }

    pub fn color3f(&self, r: f32, g: f32, b: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glColor3f(r, g, b) };
    }

    pub fn color4f(&self, r: f32, g: f32, b: f32, a: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glColor4f(r, g, b, a) };
    }

    pub fn normal3f(&self, x: f32, y: f32, z: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glNormal3f(x, y, z) };
    }

    pub fn tex_coord2f(&self, s: f32, t: f32) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glTexCoord2f(s, t) };
    }

    /// `glShadeModel`: `true` — `GL_SMOOTH`, `false` — `GL_FLAT`.
    pub fn set_smooth_shading(&self, smooth: bool) {
        // GL_SMOOTH = 0x1D01, GL_FLAT = 0x1D00 (gl.h).
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glShadeModel(if smooth { 0x1D01 } else { 0x1D00 }) };
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: gl_ctx создан нашим glInit и не переиспользован (LiveGuard
        // держится до конца Drop, поля освобождаются после тела).
        unsafe { ffi::glClose() };
        // SAFETY: zb вернул ZB_open; ZB_close освобождает zbuf и не трогает
        // внешний frame_buffer (frame_buffer_allocated == 0).
        unsafe { ffi::ZB_close(self.zb.as_ptr()) };
    }
}
