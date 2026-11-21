// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! # `tinygl` — тонкая безопасная обёртка над vendored TinyGL 0.4.1
//!
//! **PR3 — текстуры и display lists** поверх FFI/core из PR2. Upstream C 0.4.1
//! (MIT, Fabrice Bellard) компилируется как есть (один патч `zfeatures.h`, см.
//! `c_src/PATCHES.md`), а этот крейт даёт безопасный Rust-слой: контекст,
//! viewport/clear, матрицы (включая stack и `gluPerspective`-эквивалент),
//! immediate-mode примитивы, depth/cull, `ZB_resize`, текстуры и display
//! lists. Композитор здесь **не** участвует и не трогается — это headless-ядро;
//! демо `pagh-gl-demo` и PDP/1 — отдельные PR (спека
//! `docs/tinygl-port-spec.md`, §9).
//!
//! Никаких shaders/GLSL/GL 2.0+/FBO/VBO/EGL/GLX/Wayland/Mesa и никакой
//! аппаратной растеризации — только CPU. Это **не** «порт OpenGL».
//!
//! ## Текстуры: что умеет upstream 0.4.1 (и чего мы не обещаем)
//!
//! * `glTexImage2D` принимает **одно** сочетание: `GL_RGB`/`GL_UNSIGNED_BYTE`,
//!   `level = 0`, `border = 0`, `components = 3`. Всё остальное upstream
//!   обрывает через `gl_fatal_error` → `exit(1)`; наш слой такие входы не
//!   принимает вовсе (тип API), а размеры/буфер проверяет до C.
//! * Любой размер, отличный от **256×256**, upstream ресемплит **без
//!   интерполяции** в 256×256 (`gl_resizeImageNoInterpolate`, ближайший
//!   сосед по целочисленной сетке). Mipmap-уровней нет; `level != 0`
//!   невозможен. `GL_MAX_TEXTURE_SIZE` = 256 — это правда, а не пожелание.
//! * Фильтрация — **всегда** `GL_NEAREST`: `glTexParameteri` для
//!   `GL_TEXTURE_MIN_FILTER`/`MAG_FILTER` upstream молча игнорирует, значения
//!   не сохраняются. Wrap — **всегда** `GL_REPEAT` (координаты маскируются на
//!   256); `GL_TEXTURE_WRAP_S/T` принимает только `GL_REPEAT`.
//! * Режим наложения — только `GL_DECAL` (`glTexEnvi`), и в 32-битном
//!   растеризаторе он означает «тексель заменяет цвет»: цвет вершины и альфа
//!   для текстурированного треугольника **не используются**.
//! * Текстурные координаты не проходят через modelview/projection; через
//!   **texture matrix** (`glMatrixMode(GL_TEXTURE)`) они проходят, если она не
//!   единичная (`vertex.c`: `apply_texture_matrix`). По умолчанию она
//!   единичная.
//! * Имя текстуры — `u32`, но C хранит `int`; имена `> i32::MAX` наш слой
//!   отвергает. Имя `0` — «текстура по умолчанию»: `glBindTexture(0)` создаёт
//!   объект, но `glDeleteTextures(0)` в upstream оставляет висячий
//!   `current_texture`, поэтому наш `delete_textures` имя `0` не передаёт.
//! * `GL_TEXTURE_2D` можно включить только при валидной (загруженной) текстуре:
//!   иначе upstream разыменовывает `NULL` на первом заливном треугольнике.
//!
//! ## Display lists: что умеет upstream 0.4.1
//!
//! * `glNewList`/`glEndList`/`glCallList`/`glGenLists`/`glIsList` есть;
//!   `glDeleteLists` — **пустой `inline` в `gl.h`**, внешнего символа нет, и
//!   освобождения слотов в C не существует. [`Context::delete_lists`]
//!   эмулирует GL-удаление имён: имя перестаёт быть вызываемым, записанные
//!   операции C освобождает через `glNewList`+`glEndList`, но слот остаётся
//!   занятым до переопределения имени или закрытия контекста.
//! * Записанные операции нельзя хранить с клиентскими указателями:
//!   `glTexImage2D`/`glGenTextures`/`glDeleteTextures` внутри компиляции
//!   отвергаются, иначе реплей читал бы освобождённую память Rust.
//! * Перед `glCallList` наш слой проигрывает зеркало списка и отвергает то,
//!   на чём C упал бы: переполнение/недостачу матричного стека, вложенный
//!   `glBegin`, включённую текстуру без картинки на текстурном треугольнике,
//!   рекурсию списков, неизвестное имя (upstream — `exit(1)`), отравленный
//!   список. После реплея зеркало матричного режима/глубин и текстуры
//!   приводится к состоянию C.
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
mod lists;
mod surface;

pub use lists::ListMode;
pub use surface::{FbError, Surface, MAX_SURFACE_DIM};

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use lists::{CompileState, ListOp, ListRegistry};

/// Наш sanity-предел на один `glGenTextures(n, ...)`: у upstream предела нет,
/// но буфер имён — наш Vec.
const MAX_TEXTURES_PER_CALL: usize = 1024;

/// Наш предел стороны текстуры. Upstream всё равно ресемплит в 256×256, а его
/// ресемплер делает `xsize_src << 16` — при огромных размерах это int-overflow
/// (C-UB); такой размер не должен доходить до C.
const MAX_TEXTURE_DIM: u32 = 4096;

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

/// Режим матричного стека (`glMatrixMode`): три стека upstream, у каждого свой
/// предел (`32`/`8`/`8`, читаются из C в [`Context::new`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixMode {
    ModelView,
    Projection,
    Texture,
}

impl MatrixMode {
    pub(crate) fn to_gl(self) -> i32 {
        match self {
            MatrixMode::ModelView => ffi::GL_MODELVIEW,
            MatrixMode::Projection => ffi::GL_PROJECTION,
            MatrixMode::Texture => ffi::GL_TEXTURE,
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            MatrixMode::ModelView => 0,
            MatrixMode::Projection => 1,
            MatrixMode::Texture => 2,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            MatrixMode::ModelView => "modelview",
            MatrixMode::Projection => "projection",
            MatrixMode::Texture => "texture",
        }
    }

    fn matrix_pname(self) -> i32 {
        match self {
            MatrixMode::ModelView => ffi::GL_MODELVIEW_MATRIX,
            MatrixMode::Projection => ffi::GL_PROJECTION_MATRIX,
            MatrixMode::Texture => ffi::GL_TEXTURE_MATRIX,
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

    /// Примитивы, которые на реплее доходят до заливки треугольников и тем
    /// самым разыменовывают `current_texture` (только они «текстурные»;
    /// точки/линии цветные и текстуру не читают — так в upstream `clip.c`).
    pub(crate) fn rasterizes_triangles(self) -> bool {
        matches!(
            self,
            Primitive::Triangles
                | Primitive::TriangleStrip
                | Primitive::TriangleFan
                | Primitive::Quads
                | Primitive::QuadStrip
                | Primitive::Polygon
        )
    }
}

/// Включаемое состояние (`glEnable`/`glDisable`) из поддерживаемого upstream
/// подмножества. `GL_TEXTURE_2D` сюда намеренно не входит: включение текстуры
/// требует валидной (загруженной) текстуры, иначе upstream разыменовывает
/// `NULL`; это отдельные [`Context::enable_texture_2d`] /
/// [`Context::disable_texture_2d`] с `Result`.
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

/// Параметр `glTexParameteri` из поддерживаемого upstream подмножества.
///
/// Фильтры (`MinFilter`/`MagFilter`) upstream **молча игнорирует**: значения не
/// сохраняются, растеризатор всегда берёт `GL_NEAREST`. Мы всё равно шлём
/// вызов (чтобы поведение не «разошлось» с C), но принимаем только `Nearest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexturePname {
    WrapS,
    WrapT,
    MinFilter,
    MagFilter,
}

/// Значение [`TexturePname`]: единственное допустимое для каждого параметра —
/// `GL_REPEAT` для wrap и `GL_NEAREST` для фильтров.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureParam {
    Repeat,
    Nearest,
}

impl TexturePname {
    fn name(self) -> &'static str {
        match self {
            TexturePname::WrapS => "WRAP_S",
            TexturePname::WrapT => "WRAP_T",
            TexturePname::MinFilter => "MIN_FILTER",
            TexturePname::MagFilter => "MAG_FILTER",
        }
    }
}

impl TextureParam {
    fn name(self) -> &'static str {
        match self {
            TextureParam::Repeat => "REPEAT",
            TextureParam::Nearest => "NEAREST",
        }
    }
}

/// Режим `glTexEnvi(GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, ...)`; upstream
/// принимает только `GL_DECAL` (в 32-битном растеризаторе — замена цвета
/// текселем).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureEnvMode {
    Decal,
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
    /// Projection, Texture]`.
    stack_depth: Cell<[usize; 3]>,
    /// Максимум push, прочитанный из C (`glGetIntegerv`). Upstream assert'у
    /// удовлетворяет `depth + 1 < limit`, поэтому допустимо `limit - 1` push.
    stack_limit: [usize; 3],
    in_begin: Cell<bool>,
    /// Примитив открытого `glBegin` (нужен реплею списка, вызванного внутри
    /// begin: он должен оставить `in_begin`/примитив как были).
    current_primitive: Cell<Option<Primitive>>,
    /// Имя текущей текстуры (`glBindTexture`); `None` — ни один bind не было,
    /// и C-`current_texture` ещё `NULL` (любой `glTexImage2D` упал бы).
    bound_texture: Cell<Option<u32>>,
    /// Зеркало `c->texture_2d_enabled`.
    texture_enabled: Cell<bool>,
    /// Имена, для которых C создал объект текстуры (любой `glBindTexture`).
    texture_objects: RefCell<BTreeSet<u32>>,
    /// Имена с загруженной картинкой (`glTexImage2D`); только они годятся для
    /// включённого `GL_TEXTURE_2D`.
    textured_images: RefCell<BTreeSet<u32>>,
    /// Зеркало list-слотов и записанных операций (см. [`lists`]).
    lists: RefCell<ListRegistry>,
    /// Открытая компиляция списка, если есть.
    compiling: Cell<Option<CompileState>>,
    /// Открыт ли `glBegin` внутри компилируемого списка.
    list_in_begin: Cell<bool>,
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
        let mut texture_limit: i32 = 0;
        // SAFETY: gl_ctx создан строкой выше; GL_MAX_*_STACK_DEPTH — pname,
        // который get.c разбирает и пишет одно int по валидному указателю.
        unsafe {
            ffi::glGetIntegerv(ffi::GL_MAX_MODELVIEW_STACK_DEPTH, &mut modelview_limit);
            ffi::glGetIntegerv(ffi::GL_MAX_PROJECTION_STACK_DEPTH, &mut projection_limit);
            ffi::glGetIntegerv(ffi::GL_MAX_TEXTURE_STACK_DEPTH, &mut texture_limit);
        }

        Ok(Context {
            zb,
            surface,
            matrix_mode_state: Cell::new(MatrixMode::ModelView),
            stack_depth: Cell::new([0, 0, 0]),
            stack_limit: [
                usize::try_from(modelview_limit).unwrap_or(32).max(2),
                usize::try_from(projection_limit).unwrap_or(8).max(2),
                usize::try_from(texture_limit).unwrap_or(8).max(2),
            ],
            in_begin: Cell::new(false),
            current_primitive: Cell::new(None),
            bound_texture: Cell::new(None),
            texture_enabled: Cell::new(false),
            texture_objects: RefCell::new(BTreeSet::new()),
            textured_images: RefCell::new(BTreeSet::new()),
            lists: RefCell::new(ListRegistry::default()),
            compiling: Cell::new(None),
            list_in_begin: Cell::new(false),
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

    /// Открытая компиляция списка, если есть.
    fn compile_state(&self) -> Option<CompileState> {
        self.compiling.get()
    }

    /// Выполняются ли операции сейчас: вне списка или в
    /// `GL_COMPILE_AND_EXECUTE` — да; в `GL_COMPILE` — нет.
    fn executes_now(&self) -> bool {
        match self.compiling.get() {
            None => true,
            Some(state) => state.mode == ListMode::CompileAndExecute,
        }
    }

    /// Дописать операцию в зеркало компилируемого списка (если он открыт).
    fn record(&self, op: ListOp) {
        if let Some(state) = self.compiling.get() {
            self.lists.borrow_mut().record(state.id, op);
        }
    }

    /// Отметить текущий список «отравленным»: в него попало то, что на реплее
    /// упало бы, и `end_list` вернёт [`FbError::ListMalformed`].
    fn poison_current_list(&self) {
        if let Some(state) = self.compiling.get() {
            self.lists.borrow_mut().poison(state.id);
        }
    }

    /// Есть ли у текущей текстуры картинка (иначе текстурный треугольник в C
    /// разыменует `NULL`).
    fn texture_ready(&self) -> bool {
        match self.bound_texture.get() {
            Some(texture) => self.textured_images.borrow().contains(&texture),
            None => false,
        }
    }

    /// Текущая текстура, готовая к текстурным треугольникам.
    fn require_texture(&self) -> Result<u32, FbError> {
        match self.bound_texture.get() {
            None => Err(FbError::TextureNotBound),
            Some(texture) if !self.textured_images.borrow().contains(&texture) => {
                Err(FbError::TextureHasNoImage { texture })
            }
            Some(texture) => Ok(texture),
        }
    }

    fn set_bound_texture(&self, texture: u32) {
        self.bound_texture.set(Some(texture));
        self.texture_objects.borrow_mut().insert(texture);
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
    ///
    /// Внутри `GL_COMPILE`-списка C меняет режим только при реплее, поэтому
    /// [`Self::current_matrix_mode`] до `call_list` показывает **живой** режим;
    /// записанные в список push/pop учитывают режим на момент реплея.
    pub fn matrix_mode(&self, mode: MatrixMode) {
        // SAFETY: единственный живой gl_ctx создан в new(); режим — из нашего
        // enum (GL_MODELVIEW/GL_PROJECTION/GL_TEXTURE), остальное C отверг бы
        // assert'ом.
        unsafe { ffi::glMatrixMode(mode.to_gl()) };
        self.record(ListOp::MatrixMode(mode));
        if self.executes_now() {
            self.matrix_mode_state.set(mode);
        }
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
    ///
    /// Внутри `GL_COMPILE`-списка операция только записывается (C её не
    /// исполняет), поэтому предел проверяется на реплее [`Self::call_list`];
    /// `GL_COMPILE_AND_EXECUTE` исполняется сразу и проверяется здесь.
    pub fn push_matrix(&self) -> Result<(), FbError> {
        if let Some(state) = self.compile_state() {
            if state.mode == ListMode::Compile {
                // SAFETY: gl_ctx жив; в режиме компиляции gl_add_op только
                // записывает OP_PushMatrix, glopPushMatrix не вызывается.
                unsafe { ffi::glPushMatrix() };
                self.record(ListOp::PushMatrix);
                return Ok(());
            }
        }
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
        self.record(ListOp::PushMatrix);
        Ok(())
    }

    /// `glPopMatrix` с защитой от upstream `assert` на пустом стеке (та же
    /// оговорка про `GL_COMPILE`, что и у [`Self::push_matrix`]).
    pub fn pop_matrix(&self) -> Result<(), FbError> {
        if let Some(state) = self.compile_state() {
            if state.mode == ListMode::Compile {
                // SAFETY: gl_ctx жив; в режиме компиляции исполнения нет.
                unsafe { ffi::glPopMatrix() };
                self.record(ListOp::PopMatrix);
                return Ok(());
            }
        }
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
        self.record(ListOp::PopMatrix);
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
        if let Some(state) = self.compile_state() {
            if self.list_in_begin.get() {
                return Err(FbError::BeginNested);
            }
            // SAFETY: gl_ctx жив; primitive — из нашего enum (валидный GL_*);
            // вложенности нет. В режиме Compile C только записывает опкод.
            unsafe { ffi::glBegin(primitive.to_gl()) };
            self.list_in_begin.set(true);
            self.record(ListOp::Begin(primitive));
            if state.mode == ListMode::CompileAndExecute {
                self.in_begin.set(true);
                self.current_primitive.set(Some(primitive));
            }
            return Ok(());
        }
        if self.in_begin.get() {
            return Err(FbError::BeginNested);
        }
        // SAFETY: единственный живой gl_ctx создан в new(); primitive — из
        // нашего enum, значит валидный GL_*; вложенности нет (проверено).
        unsafe { ffi::glBegin(primitive.to_gl()) };
        self.in_begin.set(true);
        self.current_primitive.set(Some(primitive));
        Ok(())
    }

    /// `glEnd` с защитой от upstream `assert` без парного `glBegin`.
    pub fn end(&self) -> Result<(), FbError> {
        if let Some(state) = self.compile_state() {
            if !self.list_in_begin.get() {
                return Err(FbError::EndWithoutBegin);
            }
            // SAFETY: gl_ctx жив; begin был (проверено); в режиме Compile C
            // только записывает опкод, в CompileAndExecute — исполняет.
            unsafe { ffi::glEnd() };
            self.list_in_begin.set(false);
            self.record(ListOp::End);
            if state.mode == ListMode::CompileAndExecute {
                self.in_begin.set(false);
                self.current_primitive.set(None);
            }
            return Ok(());
        }
        if !self.in_begin.get() {
            return Err(FbError::EndWithoutBegin);
        }
        // SAFETY: in_begin означает, что upstream assert пройдёт; gl_ctx жив.
        unsafe { ffi::glEnd() };
        self.in_begin.set(false);
        self.current_primitive.set(None);
        Ok(())
    }

    /// Можно ли звать C для очередной вершины.
    ///
    /// В `GL_COMPILE` вершина **всегда** записывается: C не исполняет
    /// `glopVertex`, а на реплее begin/end уже будут записаны (или вызов
    /// произойдёт внутри живого `glBegin`, что тоже законно). В
    /// `GL_COMPILE_AND_EXECUTE` C исполняет сразу, поэтому вершина вне
    /// `glBegin` (upstream — `assert`) не передаётся в C, а отравляет список:
    /// тогда `end_list` вернёт [`FbError::ListMalformed`]. Тот же предохранитель
    /// — на включённую текстуру без картинки (upstream разыменовал бы `NULL`).
    fn vertex_may_reach_c(&self) -> bool {
        let Some(state) = self.compile_state() else {
            return true;
        };
        if state.mode == ListMode::Compile {
            return true;
        }
        if !self.list_in_begin.get() {
            self.poison_current_list();
            return false;
        }
        if self.texture_enabled.get() && !self.texture_ready() {
            self.poison_current_list();
            return false;
        }
        true
    }

    /// `glVertex3f`. Вне списка вызывать только между [`Self::begin`] и
    /// [`Self::end`] — upstream `glopVertex` содержит `assert(in_begin)`; в
    /// `GL_COMPILE`-списке вершина просто записывается, а её законность
    /// проверяет [`Self::call_list`] (список можно вызывать и внутри begin).
    pub fn vertex3f(&self, x: f32, y: f32, z: f32) {
        debug_finite(&[x, y, z], "vertex3f");
        if self.vertex_may_reach_c() {
            // SAFETY: контракт документирован; gl_ctx жив.
            unsafe { ffi::glVertex3f(x, y, z) };
            self.record(ListOp::Vertex);
        }
    }

    pub fn vertex2f(&self, x: f32, y: f32) {
        debug_finite(&[x, y], "vertex2f");
        if self.vertex_may_reach_c() {
            // SAFETY: контракт документирован; gl_ctx жив.
            unsafe { ffi::glVertex2f(x, y) };
            self.record(ListOp::Vertex);
        }
    }

    pub fn vertex3fv(&self, v: &[f32; 3]) {
        debug_finite(v, "vertex3fv");
        if self.vertex_may_reach_c() {
            // SAFETY: v — валидная ссылка на 3 float; glVertex3fv их копирует.
            unsafe { ffi::glVertex3fv(v.as_ptr()) };
            self.record(ListOp::Vertex);
        }
    }

    pub fn vertex4f(&self, x: f32, y: f32, z: f32, w: f32) {
        debug_finite(&[x, y, z, w], "vertex4f");
        if self.vertex_may_reach_c() {
            // SAFETY: единственный живой gl_ctx создан в new().
            unsafe { ffi::glVertex4f(x, y, z, w) };
            self.record(ListOp::Vertex);
        }
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

    // --- текстуры (PR3) ---------------------------------------------------

    /// `glGenTextures`: вернуть `n` новых имён.
    ///
    /// Имя — ещё не объект: `GLTexture` создаётся первым [`Self::bind_texture`].
    /// Upstream выдаёт `max(handle) + i + 1`, где `max` — максимум только по
    /// **существующим объектам**, поэтому выданные, но ни разу не привязанные
    /// имена могут быть выданы повторно (upstream-поведение; в GL имена
    /// уникальны). Наш слой заранее отвергает запрос, который переполнил бы
    /// C-`int` (upstream этого не проверяет).
    pub fn gen_textures(&self, n: usize) -> Result<Vec<u32>, FbError> {
        if self.compile_state().is_some() {
            return Err(FbError::ListClientPointer);
        }
        if n == 0 || n > MAX_TEXTURES_PER_CALL {
            return Err(FbError::BadTextureCount { count: n });
        }
        let max_object = self
            .texture_objects
            .borrow()
            .iter()
            .next_back()
            .copied()
            .unwrap_or(0);
        if u64::from(max_object) + n as u64 > i32::MAX as u64 {
            return Err(FbError::TextureNameExhausted);
        }
        let mut names = vec![0u32; n];
        // SAFETY: gl_ctx жив; `names` — валидный буфер ровно на n u32, столько
        // upstream и пишет (`glGenTextures(n, textures)`).
        unsafe { ffi::glGenTextures(n as i32, names.as_mut_ptr()) };
        Ok(names)
    }

    /// `glDeleteTextures`: удалить имена.
    ///
    /// Имя `0` не передаётся в C: upstream при удалении текущей текстуры
    /// вызывает `glBindTexture(GL_TEXTURE_2D, 0)`, а для уже связанного `0`
    /// оставляет `current_texture` висячим указателем. GL и так велит
    /// игнорировать имя `0`. Удаление **текущей** текстуры при включённом
    /// `GL_TEXTURE_2D` отвергается: C перебиндил бы `0` без картинки, и
    /// следующий треугольник упал бы.
    pub fn delete_textures(&self, textures: &[u32]) -> Result<(), FbError> {
        if self.compile_state().is_some() {
            return Err(FbError::ListClientPointer);
        }
        let mut names = Vec::new();
        for &name in textures {
            if name == 0 {
                continue;
            }
            if name > i32::MAX as u32 {
                return Err(FbError::BadTextureName { name });
            }
            if self.texture_enabled.get() && self.bound_texture.get() == Some(name) {
                return Err(FbError::TextureBoundAndEnabled { texture: name });
            }
            names.push(name);
        }
        if names.is_empty() {
            return Ok(());
        }
        // SAFETY: gl_ctx жив; `names` — валидный буфер на names.len() u32;
        // имена проверены на int-диапазон, а имя 0 отфильтровано.
        unsafe { ffi::glDeleteTextures(names.len() as i32, names.as_ptr()) };

        let mut objects = self.texture_objects.borrow_mut();
        let mut images = self.textured_images.borrow_mut();
        let bound = self.bound_texture.get();
        for &name in &names {
            objects.remove(&name);
            images.remove(&name);
            if bound == Some(name) {
                // glDeleteTextures сам биндит 0 (upstream), создавая объект 0.
                self.bound_texture.set(Some(0));
                objects.insert(0);
            }
        }
        Ok(())
    }

    /// `glBindTexture(GL_TEXTURE_2D, texture)`: сделать текстуру текущей.
    ///
    /// Имя `0` допустимо (upstream создаёт для него объект). При включённом
    /// `GL_TEXTURE_2D` привязка текстуры без картинки отвергается: следующий
    /// заливной треугольник разыменовал бы `pixmap == NULL`. Внутри
    /// `GL_COMPILE`-списка операция только записывается, а проверяется на
    /// реплее [`Self::call_list`].
    pub fn bind_texture(&self, texture: u32) -> Result<(), FbError> {
        if texture > i32::MAX as u32 {
            return Err(FbError::BadTextureName { name: texture });
        }
        let executes_now = self.executes_now();
        if executes_now
            && self.texture_enabled.get()
            && !self.textured_images.borrow().contains(&texture)
        {
            return Err(FbError::TextureHasNoImage { texture });
        }
        // SAFETY: gl_ctx жив; target — GL_TEXTURE_2D (единственный, что
        // принимает upstream); имя проверено на неотрицательность.
        unsafe { ffi::glBindTexture(ffi::GL_TEXTURE_2D, texture as i32) };
        self.record(ListOp::BindTexture(texture));
        if executes_now {
            self.set_bound_texture(texture);
        }
        Ok(())
    }

    /// `glTexImage2D(GL_TEXTURE_2D, 0, 3, w, h, 0, GL_RGB, GL_UNSIGNED_BYTE, rgb)`.
    ///
    /// `rgb` — плотные строки `w * h * 3` байт (RGB8). Upstream ресемплит
    /// любой размер в 256×256 **без интерполяции**; mipmap-уровней нет.
    /// Требуется предварительный [`Self::bind_texture`] (иначе C-указатель
    /// `current_texture` равен `NULL`). Внутри компиляции списка вызов
    /// отвергается: C записал бы сырой указатель на Rust-слайс и на реплее
    /// прочитал освобождённую память.
    pub fn tex_image_2d(&self, width: u32, height: u32, rgb: &[u8]) -> Result<(), FbError> {
        if self.compile_state().is_some() {
            return Err(FbError::ListClientPointer);
        }
        if width == 0 || height == 0 || width > MAX_TEXTURE_DIM || height > MAX_TEXTURE_DIM {
            return Err(FbError::BadTextureSize { width, height });
        }
        let expected = (width as usize) * (height as usize) * 3;
        if rgb.len() != expected {
            return Err(FbError::TextureDataLenMismatch {
                expected,
                actual: rgb.len(),
            });
        }
        if self.bound_texture.get().is_none() {
            return Err(FbError::TextureNotBound);
        }
        // SAFETY: gl_ctx жив и current_texture создан bind_texture; сочетание
        // target/level/components/border/format/type — ровно то, которое
        // glopTexImage2D разбирает (иначе был бы exit(1)); `rgb` валиден на
        // width*height*3 байт, upstream их только читает.
        unsafe {
            ffi::glTexImage2D(
                ffi::GL_TEXTURE_2D,
                0,
                3,
                width as i32,
                height as i32,
                0,
                ffi::GL_RGB,
                ffi::GL_UNSIGNED_BYTE,
                rgb.as_ptr() as *mut std::ffi::c_void,
            )
        };
        if let Some(texture) = self.bound_texture.get() {
            self.textured_images.borrow_mut().insert(texture);
            self.texture_objects.borrow_mut().insert(texture);
        }
        Ok(())
    }

    /// `glTexParameteri` из поддерживаемого upstream подмножества: `REPEAT` для
    /// `GL_TEXTURE_WRAP_S/T`, `NEAREST` для `GL_TEXTURE_MIN/MAG_FILTER`.
    ///
    /// Всё остальное вернёт [`FbError::UnsupportedTextureParameter`], а не
    /// упадёт в C: `glopTexParameter` на неизвестном pname/param делает
    /// `exit(1)` (фильтры он молча игнорирует — значения не сохраняются).
    pub fn tex_parameter(&self, pname: TexturePname, param: TextureParam) -> Result<(), FbError> {
        let (gl_pname, gl_param) = match (pname, param) {
            (TexturePname::WrapS, TextureParam::Repeat) => (ffi::GL_TEXTURE_WRAP_S, ffi::GL_REPEAT),
            (TexturePname::WrapT, TextureParam::Repeat) => (ffi::GL_TEXTURE_WRAP_T, ffi::GL_REPEAT),
            (TexturePname::MinFilter, TextureParam::Nearest) => {
                (ffi::GL_TEXTURE_MIN_FILTER, ffi::GL_NEAREST)
            }
            (TexturePname::MagFilter, TextureParam::Nearest) => {
                (ffi::GL_TEXTURE_MAG_FILTER, ffi::GL_NEAREST)
            }
            _ => {
                return Err(FbError::UnsupportedTextureParameter {
                    pname: pname.name(),
                    param: param.name(),
                })
            }
        };
        // SAFETY: gl_ctx жив; pname/param — из поддерживаемой upstream пары,
        // fatal-ветку `glopTexParameter` не задеваем.
        unsafe { ffi::glTexParameteri(ffi::GL_TEXTURE_2D, gl_pname, gl_param) };
        Ok(())
    }

    /// `glTexEnvi(GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, GL_DECAL)`.
    ///
    /// `GL_DECAL` — единственный режим upstream; в 32-битном растеризаторе он
    /// означает «тексель заменяет цвет» (цвет вершины для текстурного
    /// треугольника не используется).
    pub fn tex_env(&self, mode: TextureEnvMode) {
        let gl_mode = match mode {
            TextureEnvMode::Decal => ffi::GL_DECAL,
        };
        // SAFETY: gl_ctx жив; единственная принимаемая upstream тройка
        // target/pname/param, без fatal-ветки.
        unsafe { ffi::glTexEnvi(ffi::GL_TEXTURE_ENV, ffi::GL_TEXTURE_ENV_MODE, gl_mode) };
    }

    /// `glEnable(GL_TEXTURE_2D)` с проверкой: текстура обязана быть привязана и
    /// иметь картинку, иначе upstream разыменует `NULL` на первом текстурном
    /// треугольнике. Внутри `GL_COMPILE`-списка проверка откладывается до
    /// [`Self::call_list`] (там валидность зависит от состояния в момент вызова).
    pub fn enable_texture_2d(&self) -> Result<(), FbError> {
        if let Some(state) = self.compile_state() {
            if state.mode == ListMode::Compile {
                // SAFETY: gl_ctx жив; в режиме компиляции glopEnableDisable не
                // исполняется, опкод только записывается.
                unsafe { ffi::glEnable(ffi::GL_TEXTURE_2D) };
                self.record(ListOp::EnableTexture);
                return Ok(());
            }
        }
        self.require_texture()?;
        // SAFETY: gl_ctx жив; текстура валидна (проверено), NULL-разыменования
        // в растеризаторе не будет.
        unsafe { ffi::glEnable(ffi::GL_TEXTURE_2D) };
        self.texture_enabled.set(true);
        self.record(ListOp::EnableTexture);
        Ok(())
    }

    /// `glDisable(GL_TEXTURE_2D)`.
    pub fn disable_texture_2d(&self) {
        // SAFETY: gl_ctx жив.
        unsafe { ffi::glDisable(ffi::GL_TEXTURE_2D) };
        self.record(ListOp::DisableTexture);
        if self.executes_now() {
            self.texture_enabled.set(false);
        }
    }

    // --- display lists (PR3) ----------------------------------------------

    /// `glGenLists(range)`: выделить `range` последовательных имён; вернуть
    /// первое.
    ///
    /// Upstream возвращает `0` и при успехе, если диапазон начинается с нуля,
    /// поэтому наш слой сам считает занятость слотов (зеркало `generated`) и
    /// сверяет предсказанное имя с фактическим. Имена `>= 1024` в C не
    /// допускаются никогда: `shared_state.lists[id]` индексируется без проверки
    /// границ.
    pub fn gen_lists(&self, range: u32) -> Result<u32, FbError> {
        if self.compile_state().is_some() {
            return Err(FbError::ListClientPointer);
        }
        if range == 0 || range > ffi::MAX_DISPLAY_LISTS {
            return Err(FbError::BadListRange { range });
        }
        let predicted = self
            .lists
            .borrow()
            .first_free_run(range)
            .ok_or(FbError::ListSpaceExhausted)?;
        // SAFETY: gl_ctx жив; range <= 1024 и свободный диапазон есть;
        // upstream пишет только слоты 0..1023.
        let actual = unsafe { ffi::glGenLists(range as i32) };
        if actual != predicted {
            return Err(FbError::ListStateDiverged {
                expected: predicted,
                actual,
            });
        }
        self.lists.borrow_mut().insert_generated(predicted, range);
        Ok(predicted)
    }

    /// `glIsList` по нашему зеркалу (в debug сверяется с C).
    pub fn is_list(&self, list: u32) -> bool {
        let defined = self.lists.borrow().is_defined(list);
        if defined {
            // SAFETY: в зеркале только имена, выданные glGenLists (< 1024);
            // на них upstream glIsList отвечает без OOB.
            debug_assert_ne!(unsafe { ffi::glIsList(list) }, 0);
        }
        defined
    }

    /// `glNewList(list, mode)`: начать компиляцию.
    ///
    /// `list` обязан быть выдан [`Self::gen_lists`]. Повторное использование
    /// имени переопределяет список (upstream удаляет старый и выделяет новый).
    pub fn new_list(&self, list: u32, mode: ListMode) -> Result<(), FbError> {
        if self.compile_state().is_some() {
            return Err(FbError::ListAlreadyCompiling);
        }
        if self.in_begin.get() {
            return Err(FbError::ListDuringBegin);
        }
        if !self.lists.borrow().is_generated(list) {
            return Err(FbError::ListUndefined { list });
        }
        // SAFETY: list < 1024 (выдан нашим glGenLists), mode — из enum,
        // compile_flag свободен (проверено выше); upstream удалит прежнее
        // содержимое имени и выделит новое.
        unsafe { ffi::glNewList(list, mode.to_gl()) };
        self.lists.borrow_mut().open(list);
        self.compiling.set(Some(CompileState { id: list, mode }));
        self.list_in_begin.set(false);
        Ok(())
    }

    /// `glEndList`: завершить компиляцию.
    ///
    /// Если в список попало то, что на реплее упало бы (вершина вне `glBegin`,
    /// открытый begin), возвращается [`FbError::ListMalformed`]: список
    /// остаётся определённым, но [`Self::call_list`] его не вызывает.
    pub fn end_list(&self) -> Result<(), FbError> {
        let Some(state) = self.compile_state() else {
            return Err(FbError::ListNotCompiling);
        };
        if self.list_in_begin.get() {
            return Err(FbError::ListBeginOpen { list: state.id });
        }
        // SAFETY: compile_flag выставлен new_list; gl_ctx жив; upstream только
        // допишет OP_EndList и сбросит compile_flag.
        unsafe { ffi::glEndList() };
        let poisoned = self.lists.borrow_mut().finish(state.id);
        self.compiling.set(None);
        self.list_in_begin.set(false);
        if poisoned {
            Err(FbError::ListMalformed { list: state.id })
        } else {
            Ok(())
        }
    }

    /// `glCallList(list)`: проиграть список.
    ///
    /// Перед вызовом C зеркало списка проигрывается [`lists::simulate`]:
    /// проверяются неизвестное имя (upstream — `exit(1)`), рекурсия, глубины
    /// матричных стеков, вложенность `glBegin` и валидность текстуры на
    /// текстурных треугольниках. После успешного реплея живые зеркала
    /// матричного режима/глубин и текстуры приводятся к состоянию C.
    pub fn call_list(&self, list: u32) -> Result<(), FbError> {
        if let Some(state) = self.compile_state() {
            if state.mode == ListMode::Compile {
                // C только записывает OP_CallList; содержимое вложенного
                // списка проверится при вызове внешнего.
                // SAFETY: gl_ctx жив; в режиме компиляции glopCallList не
                // исполняется (иначе неизвестное имя дало бы exit(1)).
                unsafe { ffi::glCallList(list) };
                self.record(ListOp::CallList(list));
                return Ok(());
            }
            self.simulate_and_apply(list)?;
            // SAFETY: симуляция подтвердила, что реплей не упрётся в
            // assert/fatal/NULL; список определён, gl_ctx жив.
            unsafe { ffi::glCallList(list) };
            self.record(ListOp::CallList(list));
            return Ok(());
        }
        self.simulate_and_apply(list)?;
        // SAFETY: та же проверка; live in_begin/primitive сохранены.
        unsafe { ffi::glCallList(list) };
        Ok(())
    }

    /// `glDeleteLists(first, range)` — **эмуляция**: в upstream 0.4.1
    /// `glDeleteLists` это пустой `inline` в `gl.h`, внешнего символа нет, и
    /// освобождения C-слота не существует.
    ///
    /// Мы убираем имена из зеркала (после этого [`Self::is_list`] ложно,
    /// [`Self::call_list`] возвращает [`FbError::ListUndefined`]) и заменяем
    /// записанные операции пустым списком через `glNewList` + `glEndList`,
    /// освобождая их буферы. Сам слот в C остаётся занятым до переопределения
    /// имени или закрытия контекста — это названное ограничение, а не утечка
    /// Rust-памяти. Имена, которые не выдавал [`Self::gen_lists`], игнорируются
    /// (как в GL).
    pub fn delete_lists(&self, first: u32, range: u32) -> Result<(), FbError> {
        if self.compile_state().is_some() {
            return Err(FbError::ListClientPointer);
        }
        if range == 0 {
            return Ok(());
        }
        let end = first.saturating_add(range).min(ffi::MAX_DISPLAY_LISTS);
        for list in first..end {
            if !self.lists.borrow().is_defined(list) {
                continue;
            }
            // SAFETY: list определён (выдан glGenLists, < 1024), compile_flag
            // свободен (проверено выше); пара NewList/EndList заменяет
            // записанные операции пустым списком и освобождает их буферы.
            unsafe {
                ffi::glNewList(list, ffi::GL_COMPILE);
                ffi::glEndList();
            }
            self.lists.borrow_mut().forget(list);
        }
        Ok(())
    }

    /// Проиграть зеркало списка и применить его итог к живым зеркалам.
    fn simulate_and_apply(&self, list: u32) -> Result<(), FbError> {
        let mut state = lists::SimState {
            depth: self.stack_depth.get(),
            mode: self.matrix_mode_state.get(),
            texture_enabled: self.texture_enabled.get(),
            bound_texture: self.bound_texture.get(),
            in_begin: self.in_begin.get(),
            begin_primitive: self.current_primitive.get(),
        };
        let mut created = Vec::new();
        {
            let registry = self.lists.borrow();
            let textured = self.textured_images.borrow();
            let mut call_stack = Vec::new();
            lists::simulate(
                &registry,
                list,
                &mut state,
                &self.stack_limit,
                &textured,
                &mut created,
                &mut call_stack,
            )?;
        }
        self.stack_depth.set(state.depth);
        self.matrix_mode_state.set(state.mode);
        self.texture_enabled.set(state.texture_enabled);
        self.bound_texture.set(state.bound_texture);
        self.in_begin.set(state.in_begin);
        self.current_primitive.set(state.begin_primitive);
        let mut objects = self.texture_objects.borrow_mut();
        for texture in created {
            objects.insert(texture);
        }
        Ok(())
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
        if self.compile_state().is_some() {
            // ZB_resize — не GL-операция: она не записывается в список и
            // поменяла бы буфер прямо во время компиляции.
            return Err(FbError::ListClientPointer);
        }
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
