// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! # `tinygl` — тонкая безопасная обёртка над vendored TinyGL 0.4.1
//!
//! **PR2 — FFI core.** Upstream C 0.4.1 (MIT, Fabrice Bellard) компилируется
//! как есть (один патч `zfeatures.h`, см. `c_src/PATCHES.md`), а этот крейт
//! даёт безопасный Rust-слой: контекст, viewport/clear, матрицы (включая
//! stack и `gluPerspective`-эквивалент), immediate-mode примитивы, depth/cull
//! и `ZB_resize`. Композитор здесь **не** участвует и не трогается — это
//! headless-ядро; демо `pagh-gl-demo` и PDP/1 — отдельные PR (спека
//! `docs/tinygl-port-spec.md`, §9).
//!
//! Никаких shaders/GLSL/GL 2.0+/FBO/VBO/EGL/GLX/Wayland/Mesa и никакой
//! аппаратной растеризации — только CPU. Это **не** «порт OpenGL».
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
//! * depth test реально влияет только на линии; для залитых треугольников
//!   Z-тест выполняется **всегда** независимо от `GL_DEPTH_TEST`. `glDepthMask`/
//!   `glDepthFunc` в 0.4.1 отсутствуют — строго upstream, без форка;
//! * culling — только `GL_CULL_FACE`/`glCullFace`/`glFrontFace`; режим
//!   `GL_FRONT_AND_BACK` отбрасывает все треугольники (upstream так);
//! * **один `Context` на процесс**: `gl_ctx` в TinyGL — глобальный указатель
//!   (`init.c`), поэтому `Context` не `Send`/`Sync`, а второй живой контекст
//!   возвращает [`FbError::ContextAlreadyLive`];
//! * внутренний fatal TinyGL (`gl_fatal_error` в `error.c`) вызывает `exit(1)`
//!   с сообщением в stderr — в v1 не патчим (upstream behaviour). Поэтому наш
//!   слой сам отвергает входы, которые привели бы к fatal/`assert`: плохие
//!   размеры, пустой/полный матричный стек, `glEnd` без `glBegin`, вырожденный
//!   `frustum`, NaN/∞ (C-каст float→int — UB);
//! * **`glGetError` в upstream нет**: ошибка либо невозможна, либо фатальна.
//!   [`Context::get_error`] — совместимость-заглушка, всегда `NoError`;
//! * **`glOrtho` в upstream — пустой `inline`** в `gl.h` (внешнего символа
//!   нет): не биндится. `perspective()` — наш f64-эквивалент `gluPerspective`
//!   поверх `glFrustum`, а не upstream-функция.
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
//! musl-tools`), если ничего нет.

mod ffi;
mod surface;

pub use surface::{FbError, Surface, MAX_SURFACE_DIM};

use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

/// Дайджест пикселей, которым PR1/PR2 фиксирует upstream-поведение.
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

/// Debug-проверка «в float нет NaN/∞» для горячих immediate-mode вызовов.
///
/// В release эти проверки не выполняются (это попиксельный/повершинный путь);
/// контракт «вершины/цвета конечны» документирован в README, а валидируемые
/// state-вызовы (`translate`, `frustum`, матрицы) возвращают
/// [`FbError::NonFinite`].
#[inline(always)]
fn debug_finite(values: &[f32], what: &str) {
    debug_assert!(
        values.iter().all(|value| value.is_finite()),
        "{what} must be finite (NaN/inf reaches C float→int cast, which is UB)"
    );
}

/// Режим матричного стека (`glMatrixMode`). `GL_TEXTURE` появится вместе с
/// текстурами (PR3); в PR2 доступны только эти два режима.
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

    fn index(self) -> usize {
        match self {
            MatrixMode::ModelView => 0,
            MatrixMode::Projection => 1,
        }
    }

    fn name(self) -> &'static str {
        match self {
            MatrixMode::ModelView => "modelview",
            MatrixMode::Projection => "projection",
        }
    }

    fn matrix_pname(self) -> i32 {
        match self {
            MatrixMode::ModelView => ffi::GL_MODELVIEW_MATRIX,
            MatrixMode::Projection => ffi::GL_PROJECTION_MATRIX,
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

/// Включаемое состояние (`glEnable`/`glDisable`) из поддерживаемого upstream
/// подмножества, нужного PR2. Освещение/текстуры — PR3+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    DepthTest,
    CullFace,
}

impl Capability {
    fn to_gl(self) -> i32 {
        match self {
            Capability::DepthTest => ffi::GL_DEPTH_TEST,
            Capability::CullFace => ffi::GL_CULL_FACE,
        }
    }
}

/// Какие грани отбрасывать (`glCullFace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CullFace {
    Front,
    Back,
    FrontAndBack,
}

impl CullFace {
    fn to_gl(self) -> i32 {
        match self {
            CullFace::Front => ffi::GL_FRONT,
            CullFace::Back => ffi::GL_BACK,
            CullFace::FrontAndBack => ffi::GL_FRONT_AND_BACK,
        }
    }
}

/// Направление обхода, считающееся лицевым (`glFrontFace`). По умолчанию в
/// GL — `Ccw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontFace {
    Ccw,
    Cw,
}

impl FrontFace {
    fn to_gl(self) -> i32 {
        match self {
            FrontFace::Ccw => ffi::GL_CCW,
            FrontFace::Cw => ffi::GL_CW,
        }
    }
}

/// Текущий viewport (`glGetIntegerv(GL_VIEWPORT)`), прочитанный из C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Ошибка GL-состояния. **В TinyGL 0.4.1 нет `glGetError` и нет error-флага**
/// (`src/` не содержит ни символа `glGetError`, ни `GL_NO_ERROR`; ошибки либо
/// невозможны, либо фатальны). Поэтому [`Context::get_error`] всегда возвращает
/// `NoError` — это документированная совместимость, а не проверка состояния.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlError {
    NoError,
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
    matrix_mode_state: Cell<MatrixMode>,
    /// Текущая глубина push (число нажатий поверх базы) для `[ModelView,
    /// Projection]`.
    stack_depth: Cell<[usize; 2]>,
    /// Максимум push, прочитанный из C (`glGetIntegerv`). Upstream assert'у
    /// удовлетворяет `depth + 1 < limit`, поэтому допустимо `limit - 1` push.
    stack_limit: [usize; 2],
    in_begin: Cell<bool>,
    _live: LiveGuard,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'a> Context<'a> {
    /// Открыть `ZBuffer` поверх плотного буфера и создать `glInit`-контекст.
    ///
    /// Буфер уже провалидирован [`Surface::new`]; здесь остаётся только
    /// `ZB_open`, `glInit` и чтение пределов матричного стека для защиты от
    /// upstream-`assert` при `glPushMatrix`.
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

        let mut modelview_limit: i32 = 0;
        let mut projection_limit: i32 = 0;
        // SAFETY: gl_ctx создан строкой выше; GL_MAX_*_STACK_DEPTH — pname,
        // который get.c разбирает и пишет одно int по валидному указателю.
        unsafe {
            ffi::glGetIntegerv(ffi::GL_MAX_MODELVIEW_STACK_DEPTH, &mut modelview_limit);
            ffi::glGetIntegerv(ffi::GL_MAX_PROJECTION_STACK_DEPTH, &mut projection_limit);
        }

        Ok(Context {
            zb,
            surface,
            matrix_mode_state: Cell::new(MatrixMode::ModelView),
            stack_depth: Cell::new([0, 0]),
            stack_limit: [
                usize::try_from(modelview_limit).unwrap_or(32).max(2),
                usize::try_from(projection_limit).unwrap_or(8).max(2),
            ],
            in_begin: Cell::new(false),
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

    /// `glViewport`. В отличие от PR1, проверяет вход: upstream `glopViewport`
    /// вызывает `gl_fatal_error` → `exit(1)` при стороне `<= 0`, поэтому
    /// нулевой/огромный viewport — наш [`FbError::BadViewport`].
    pub fn set_viewport(&self, x: i32, y: i32, width: u32, height: u32) -> Result<(), FbError> {
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(FbError::BadViewport { width, height });
        }
        // SAFETY: единственный живой gl_ctx создан в new(); размеры проверены
        // и помещаются в int (MAX_SURFACE_DIM << i32::MAX).
        unsafe { ffi::glViewport(x, y, width as i32, height as i32) };
        Ok(())
    }

    /// Прочитать текущий viewport из C.
    pub fn viewport(&self) -> Viewport {
        let mut values = [0i32; 4];
        // SAFETY: gl_ctx жив; GL_VIEWPORT — поддерживаемый get.c pname, который
        // пишет ровно 4 int в валидный массив.
        unsafe { ffi::glGetIntegerv(ffi::GL_VIEWPORT, values.as_mut_ptr()) };
        Viewport {
            x: values[0],
            y: values[1],
            width: values[2].max(0) as u32,
            height: values[3].max(0) as u32,
        }
    }

    /// Максимум глубины modelview-стека, как его сообщает C (обычно 32).
    pub fn max_modelview_stack_depth(&self) -> usize {
        self.stack_limit[MatrixMode::ModelView.index()]
    }

    /// Максимум глубины projection-стека (обычно 8).
    pub fn max_projection_stack_depth(&self) -> usize {
        self.stack_limit[MatrixMode::Projection.index()]
    }

    /// `GL_MAX_LIGHTS` (у TinyGL 0.4.1 — 16).
    pub fn max_lights(&self) -> i32 {
        self.get_int(ffi::GL_MAX_LIGHTS)
    }

    /// `GL_MAX_TEXTURE_SIZE` (`get.c` отдаёт 256).
    pub fn max_texture_size(&self) -> i32 {
        self.get_int(ffi::GL_MAX_TEXTURE_SIZE)
    }

    fn get_int(&self, pname: i32) -> i32 {
        let mut value: i32 = 0;
        // SAFETY: gl_ctx жив; вызывающие передают только pname, которые get.c
        // поддерживает и пишет одним int по валидному указателю (неизвестный
        // pname дал бы fatal_error/exit).
        unsafe { ffi::glGetIntegerv(pname, &mut value) };
        value
    }

    /// `glClearColor` (float, как в GL). Альфа TinyGL в 32bpp-очистке не
    /// используется, но хранится в состоянии.
    pub fn clear_color(&self, r: f32, g: f32, b: f32, a: f32) {
        debug_finite(&[r, g, b, a], "clear colour");
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

    /// Переключить матрицу, в которую пишут последующие операции.
    pub fn matrix_mode(&self, mode: MatrixMode) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glMatrixMode(mode.to_gl()) };
        self.matrix_mode_state.set(mode);
    }

    /// Текущий режим матрицы (наше зеркало `glMatrixMode`).
    pub fn current_matrix_mode(&self) -> MatrixMode {
        self.matrix_mode_state.get()
    }

    pub fn load_identity(&self) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glLoadIdentity() };
    }

    /// Проверить и загрузить матрицу (column-major, как в GL). NaN/∞
    /// отвергаются: они дошли бы до C-преобразования float→int (UB).
    pub fn load_matrix(&self, matrix: &[f32; 16]) -> Result<(), FbError> {
        if matrix.iter().any(|value| !value.is_finite()) {
            return Err(FbError::NonFinite { what: "matrix" });
        }
        // SAFETY: matrix — валидная ссылка на 16 float; glLoadMatrixf их
        // копирует. gl_ctx жив.
        unsafe { ffi::glLoadMatrixf(matrix.as_ptr()) };
        Ok(())
    }

    /// Умножить текущую матрицу на заданную (column-major).
    pub fn mult_matrix(&self, matrix: &[f32; 16]) -> Result<(), FbError> {
        if matrix.iter().any(|value| !value.is_finite()) {
            return Err(FbError::NonFinite { what: "matrix" });
        }
        // SAFETY: matrix — валидная ссылка на 16 float; glMultMatrixf их
        // копирует. gl_ctx жив.
        unsafe { ffi::glMultMatrixf(matrix.as_ptr()) };
        Ok(())
    }

    /// `glPushMatrix` с защитой от upstream `assert`: стек исчерпан — наш
    /// [`FbError::MatrixStackOverflow`], а не C-`abort`.
    pub fn push_matrix(&self) -> Result<(), FbError> {
        let mode = self.matrix_mode_state.get();
        let index = mode.index();
        let mut depth = self.stack_depth.get();
        if depth[index] + 1 >= self.stack_limit[index] {
            return Err(FbError::MatrixStackOverflow { mode: mode.name() });
        }
        // SAFETY: глубина проверена по пределу, прочитанному из C; gl_ctx жив.
        unsafe { ffi::glPushMatrix() };
        depth[index] += 1;
        self.stack_depth.set(depth);
        Ok(())
    }

    /// `glPopMatrix` с защитой от upstream `assert` на пустом стеке.
    pub fn pop_matrix(&self) -> Result<(), FbError> {
        let mode = self.matrix_mode_state.get();
        let index = mode.index();
        let mut depth = self.stack_depth.get();
        if depth[index] == 0 {
            return Err(FbError::MatrixStackUnderflow { mode: mode.name() });
        }
        // SAFETY: глубина > 0, значит upstream assert пройдёт; gl_ctx жив.
        unsafe { ffi::glPopMatrix() };
        depth[index] -= 1;
        self.stack_depth.set(depth);
        Ok(())
    }

    pub fn frustum(
        &self,
        left: f64,
        right: f64,
        bottom: f64,
        top: f64,
        near: f64,
        far: f64,
    ) -> Result<(), FbError> {
        let values = [left, right, bottom, top, near, far];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(FbError::NonFinite { what: "frustum" });
        }
        if right == left {
            return Err(FbError::BadProjection {
                reason: "right must differ from left",
            });
        }
        if top == bottom {
            return Err(FbError::BadProjection {
                reason: "top must differ from bottom",
            });
        }
        if near <= 0.0 {
            return Err(FbError::BadProjection {
                reason: "near must be > 0",
            });
        }
        if far <= near {
            return Err(FbError::BadProjection {
                reason: "far must be > near",
            });
        }
        // SAFETY: единственный живой gl_ctx создан в new(); параметры конечны,
        // делители (right-left)/(top-bottom)/(far-near) ненулевые.
        unsafe { ffi::glFrustum(left, right, bottom, top, near, far) };
        Ok(())
    }

    /// Эквивалент `gluPerspective` (в TinyGL 0.4.1 GLU нет): считает границы
    /// через `tan(fovy/2)` и делегирует [`Self::frustum`]. Угол — в градусах.
    pub fn perspective(
        &self,
        fovy_degrees: f64,
        aspect: f64,
        near: f64,
        far: f64,
    ) -> Result<(), FbError> {
        if ![fovy_degrees, aspect, near, far]
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(FbError::NonFinite {
                what: "perspective",
            });
        }
        if !(0.0 < fovy_degrees && fovy_degrees < 180.0) {
            return Err(FbError::BadPerspective {
                reason: "fovy must be in (0, 180) degrees",
            });
        }
        if aspect <= 0.0 {
            return Err(FbError::BadPerspective {
                reason: "aspect must be > 0",
            });
        }
        if near <= 0.0 {
            return Err(FbError::BadPerspective {
                reason: "near must be > 0",
            });
        }
        if far <= near {
            return Err(FbError::BadPerspective {
                reason: "far must be > near",
            });
        }
        let top = near * (fovy_degrees.to_radians() / 2.0).tan();
        let right = top * aspect;
        self.frustum(-right, right, -top, top, near, far)
    }

    pub fn translate(&self, x: f32, y: f32, z: f32) -> Result<(), FbError> {
        if ![x, y, z].iter().all(|value| value.is_finite()) {
            return Err(FbError::NonFinite { what: "translate" });
        }
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glTranslatef(x, y, z) };
        Ok(())
    }

    /// Поворот на `angle` градусов вокруг оси `(x, y, z)`. Нулевая ось —
    /// единичная матрица (так в upstream, `matrix.c`); NaN/∞ отвергаются.
    pub fn rotate(&self, angle: f32, x: f32, y: f32, z: f32) -> Result<(), FbError> {
        if ![angle, x, y, z].iter().all(|value| value.is_finite()) {
            return Err(FbError::NonFinite { what: "rotate" });
        }
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glRotatef(angle, x, y, z) };
        Ok(())
    }

    pub fn scale(&self, x: f32, y: f32, z: f32) -> Result<(), FbError> {
        if ![x, y, z].iter().all(|value| value.is_finite()) {
            return Err(FbError::NonFinite { what: "scale" });
        }
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glScalef(x, y, z) };
        Ok(())
    }

    /// Прочитать матрицу текущего режима (column-major, 16 float).
    pub fn get_matrix(&self, mode: MatrixMode) -> [f32; 16] {
        let mut matrix = [0f32; 16];
        // SAFETY: gl_ctx жив; pname матрицы поддерживается get.c, который
        // пишет 16 float в буфер валидного размера.
        unsafe { ffi::glGetFloatv(mode.matrix_pname(), matrix.as_mut_ptr()) };
        matrix
    }

    /// `glBegin` с защитой от upstream `assert` на вложенный begin.
    pub fn begin(&self, primitive: Primitive) -> Result<(), FbError> {
        if self.in_begin.get() {
            return Err(FbError::BeginNested);
        }
        // SAFETY: единственный живой gl_ctx создан в new(); primitive — из
        // нашего enum, значит валидный GL_*; вложенности нет (проверено).
        unsafe { ffi::glBegin(primitive.to_gl()) };
        self.in_begin.set(true);
        Ok(())
    }

    /// `glEnd` с защитой от upstream `assert` без парного `glBegin`.
    pub fn end(&self) -> Result<(), FbError> {
        if !self.in_begin.get() {
            return Err(FbError::EndWithoutBegin);
        }
        // SAFETY: in_begin означает, что upstream assert пройдёт; gl_ctx жив.
        unsafe { ffi::glEnd() };
        self.in_begin.set(false);
        Ok(())
    }

    /// `glVertex3f`. Вызывать только между [`Self::begin`] и [`Self::end`] —
    /// upstream `glopVertex` содержит `assert(in_begin)`.
    pub fn vertex3f(&self, x: f32, y: f32, z: f32) {
        debug_finite(&[x, y, z], "vertex3f");
        // SAFETY: контракт документирован; gl_ctx жив.
        unsafe { ffi::glVertex3f(x, y, z) };
    }

    pub fn vertex2f(&self, x: f32, y: f32) {
        debug_finite(&[x, y], "vertex2f");
        // SAFETY: контракт документирован; gl_ctx жив.
        unsafe { ffi::glVertex2f(x, y) };
    }

    pub fn vertex3fv(&self, v: &[f32; 3]) {
        debug_finite(v, "vertex3fv");
        // SAFETY: v — валидная ссылка на 3 float; glVertex3fv их копирует.
        unsafe { ffi::glVertex3fv(v.as_ptr()) };
    }

    pub fn vertex4f(&self, x: f32, y: f32, z: f32, w: f32) {
        debug_finite(&[x, y, z, w], "vertex4f");
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glVertex4f(x, y, z, w) };
    }

    pub fn color3f(&self, r: f32, g: f32, b: f32) {
        debug_finite(&[r, g, b], "color3f");
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glColor3f(r, g, b) };
    }

    pub fn color3fv(&self, v: &[f32; 3]) {
        debug_finite(v, "color3fv");
        // SAFETY: v — валидная ссылка на 3 float; glColor3fv читает и копирует.
        unsafe { ffi::glColor3fv(v.as_ptr()) };
    }

    pub fn color4f(&self, r: f32, g: f32, b: f32, a: f32) {
        debug_finite(&[r, g, b, a], "color4f");
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glColor4f(r, g, b, a) };
    }

    pub fn color4fv(&self, v: &[f32; 4]) {
        debug_finite(v, "color4fv");
        // SAFETY: v — валидная ссылка на 4 float; glColor4fv читает и копирует.
        unsafe { ffi::glColor4fv(v.as_ptr()) };
    }

    pub fn normal3f(&self, x: f32, y: f32, z: f32) {
        debug_finite(&[x, y, z], "normal3f");
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glNormal3f(x, y, z) };
    }

    pub fn normal3fv(&self, v: &[f32; 3]) {
        debug_finite(v, "normal3fv");
        // SAFETY: v — валидная ссылка на 3 float; glNormal3fv читает и копирует.
        unsafe { ffi::glNormal3fv(v.as_ptr()) };
    }

    pub fn tex_coord2f(&self, s: f32, t: f32) {
        debug_finite(&[s, t], "tex_coord2f");
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glTexCoord2f(s, t) };
    }

    pub fn tex_coord2fv(&self, v: &[f32; 2]) {
        debug_finite(v, "tex_coord2fv");
        // SAFETY: v — валидная ссылка на 2 float; glTexCoord2fv читает и копирует.
        unsafe { ffi::glTexCoord2fv(v.as_ptr()) };
    }

    pub fn tex_coord4f(&self, s: f32, t: f32, r: f32, q: f32) {
        debug_finite(&[s, t, r, q], "tex_coord4f");
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glTexCoord4f(s, t, r, q) };
    }

    /// `glShadeModel`: `true` — `GL_SMOOTH`, `false` — `GL_FLAT`.
    pub fn set_smooth_shading(&self, smooth: bool) {
        // GL_SMOOTH = 0x1D01, GL_FLAT = 0x1D00 (gl.h).
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glShadeModel(if smooth { 0x1D01 } else { 0x1D00 }) };
    }

    pub fn enable(&self, capability: Capability) {
        // SAFETY: единственный живой gl_ctx создан в new(); cap — из нашего
        // enum, upstream игнорирует неизвестные, но мы их и не шлём.
        unsafe { ffi::glEnable(capability.to_gl()) };
    }

    pub fn disable(&self, capability: Capability) {
        // SAFETY: единственный живой gl_ctx создан в new().
        unsafe { ffi::glDisable(capability.to_gl()) };
    }

    /// `glCullFace` (действует только при включённом [`Capability::CullFace`]).
    pub fn set_cull_face(&self, face: CullFace) {
        // SAFETY: грань — из нашего enum (GL_FRONT/GL_BACK/GL_FRONT_AND_BACK),
        // остальное upstream отверг бы assert'ом.
        unsafe { ffi::glCullFace(face.to_gl()) };
    }

    /// `glFrontFace`: какое направление обхода считать лицевым.
    pub fn set_front_face(&self, winding: FrontFace) {
        // SAFETY: режим — GL_CCW/GL_CW, оба разбираются upstream без assert.
        unsafe { ffi::glFrontFace(winding.to_gl()) };
    }

    /// Совместимость-заглушка: в TinyGL 0.4.1 **нет** `glGetError`. Возвращает
    /// `NoError` всегда; ошибки в upstream либо невозможны, либо фатальны
    /// (`gl_fatal_error` → `exit(1)`), а невалидные входы отвергает наш слой.
    pub fn get_error(&self) -> GlError {
        GlError::NoError
    }

    /// Переконфигурировать тот же внешний буфер на новые размеры через
    /// `ZB_resize` и обновить viewport. Площадь не может превышать исходную
    /// ёмкость `Surface` (буфер принадлежит вызывающему и не растёт); для
    /// роста нужен новый `Surface`/`Context` (новый memfd — путь композитора).
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), FbError> {
        self.surface.check_resize(width, height)?;
        // Проверяем viewport до мутации состояния: те же границы, поэтому
        // после этого вызова ошибки быть не может.
        self.set_viewport(0, 0, width, height)?;
        let frame_buffer = self.surface.as_mut_ptr();
        // SAFETY: zb создан ZB_open и жив; frame_buffer — тот же внешний
        // буфер, что передан в ZB_open, и жив, пока жив Surface; размеры
        // проверены (1..=MAX_SURFACE_DIM, w%4==0), площадь <= ёмкости.
        unsafe {
            ffi::ZB_resize(
                self.zb.as_ptr(),
                frame_buffer.cast(),
                width as i32,
                height as i32,
            )
        };
        self.surface.apply_resize(width, height);
        Ok(())
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
