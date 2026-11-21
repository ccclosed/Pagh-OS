// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Пиксельная поверхность, в которую рендерит TinyGL, и общий тип ошибок.
//!
//! TinyGL **не выражает stride**: `zbuffer.c` считает
//! `linesize = (xsize * PSZB + 3) & ~3`, а при `PSZB = 4` (RENDER_BITS 32)
//! это ровно `width * 4` байт. Паддинга, как в PDP/1 (`stride` до `+4096`),
//! быть не может. Поэтому `Surface` принимает только **плотный** буфер:
//! `pixels.len() == width * height`; всё остальное — `FbError` до вызова C,
//! а не молчаливое ужимание/обрезка.
//!
//! Вторая upstream-особенность: `ZB_resize` делает `xsize &= ~3`, то есть
//! ширина, не кратная 4, молча теряет правые пиксели. `Surface::new` отвергает
//! такую ширину заранее.
//!
//! ## Resize
//!
//! Буфер принадлежит вызывающему (`&mut [u32]`), поэтому увеличить его из
//! `Context` нельзя: [`Surface::check_resize`] разрешает только размеры, чья
//! площадь не превышает **исходную** ёмкость буфера. Для настоящего роста
//! (новый memfd на большее окно) нужен новый `Surface` + `Context` — именно
//! так работает путь композитора (спека §4.4); `ZB_resize` здесь — для
//! уменьшения/смены формы в пределах той же памяти.

use std::fmt;

/// Предел стороны поверхности. Совпадает с `MAX_SURFACE_DIM` композитора
/// (`rust-apps/pagh-desktop/src/surface.rs`), чтобы GL-клиент не мог заказать
/// окно, которое композитор всё равно отвергнет.
pub const MAX_SURFACE_DIM: u32 = 4096;

/// Ошибка валидации поверхности, состояния и параметров GL-обёртки. Все
/// варианты — **наши** отказы до опасного входа в C: ни один не полагается на
/// `gl_fatal_error`/`exit(1)` и не приводит к C-`assert`.
///
/// Имя `FbError` историческое (PR1 проверял только framebuffer); в PR2 тип
/// стал общим для крейта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbError {
    /// Ширина не кратна 4 (TinyGL `ZB_resize` молча срезал бы `xsize & ~3`).
    WidthNotMultipleOfFour { width: u32 },
    /// Нулевая или слишком большая сторона (`> MAX_SURFACE_DIM`).
    BadDimensions { width: u32, height: u32 },
    /// `pixels.len() != width * height` — буфер не плотный (`stride != w*4`).
    BufferLenMismatch { expected: usize, actual: usize },
    /// `resize` в площадь больше исходного буфера: памяти на такой размер нет.
    ResizeTooSmall { required: usize, available: usize },
    /// `glViewport` с нулевой/слишком большой стороной (upstream — `exit(1)`).
    BadViewport { width: u32, height: u32 },
    /// Матричный стек исчерпан (upstream `glPushMatrix` — C-`assert`).
    MatrixStackOverflow { mode: &'static str },
    /// `glPopMatrix` на пустом стеке (upstream — C-`assert`).
    MatrixStackUnderflow { mode: &'static str },
    /// NaN/∞ в валидируемом параметре (матрица, трансформация, projection):
    /// C-каст float→int — UB. Вершины/цвета на горячем пути проверяются
    /// `debug_assert!` (см. README, «ограничения»).
    NonFinite { what: &'static str },
    /// Вырожденный `glFrustum` (нулевой размер/неположительный near).
    BadProjection { reason: &'static str },
    /// Невалидные параметры `perspective` (наш GLU-эквивалент).
    BadPerspective { reason: &'static str },
    /// `begin` при уже открытом `glBegin` (upstream — C-`assert`).
    BeginNested,
    /// `end` без `begin` (upstream — C-`assert`).
    EndWithoutBegin,
    /// `ZB_open` вернул `NULL` (не удалось выделить Z-буфер).
    OpenFailed,
    /// В процессе уже живёт `Context`: `gl_ctx` у TinyGL глобальный.
    ContextAlreadyLive,

    // --- текстуры (PR3) ---------------------------------------------------
    /// `glGenTextures(0)` или запрос больше нашего sanity-предела.
    BadTextureCount { count: usize },
    /// Имя текстуры не влезает в C-`int` (upstream хранит handle как `int` и
    /// делает `assert(texture >= 0)`; отрицательный handle — OOB в хэш-таблице).
    BadTextureName { name: u32 },
    /// Сторона текстуры `0` или больше нашего предела (всё равно ресемплится
    /// в 256×256; предел не даёт integer overflow в C-ресемплере).
    BadTextureSize { width: u32, height: u32 },
    /// `rgb.len() != width * height * 3` — строки `GL_RGB` обязаны быть плотными.
    TextureDataLenMismatch { expected: usize, actual: usize },
    /// `glTexImage2D` без предварительного `glBindTexture`: C-`current_texture`
    /// ещё `NULL`, и upstream разыменовал бы его.
    TextureNotBound,
    /// У текстуры нет загруженной картинки: включённый `GL_TEXTURE_2D` или
    /// `glTexImage2D` на ней привёл бы к разыменованию `pixmap == NULL`.
    TextureHasNoImage { texture: u32 },
    /// Попытка удалить текущую текстуру при включённом `GL_TEXTURE_2D`: C
    /// перебиндил бы `0` без картинки, и следующий треугольник упал бы.
    TextureBoundAndEnabled { texture: u32 },
    /// `glGenTextures` переполнил бы C-`int` (`max(handle) + i + 1`).
    TextureNameExhausted,
    /// Пара `glTexParameteri`, которую upstream либо отверг бы (`exit(1)`),
    /// либо молча проигнорировал (фильтры всегда NEAREST, wrap всегда REPEAT).
    UnsupportedTextureParameter {
        pname: &'static str,
        param: &'static str,
    },

    // --- display lists (PR3) ---------------------------------------------
    /// `range == 0` или больше `MAX_DISPLAY_LISTS` (1024 слота upstream).
    BadListRange { range: u32 },
    /// В `shared_state.lists` upstream-размера (1024) нет непрерывного
    /// диапазона нужной длины.
    ListSpaceExhausted,
    /// Наше зеркало занятости слотов разошлось с C. Должно быть невозможно;
    /// если случилось — дальше звать `glGenLists`/`glCallList` нельзя.
    ListStateDiverged { expected: u32, actual: u32 },
    /// Имя не выдавалось `glGenLists` (upstream `glCallList` → `exit(1)`,
    /// а `glCallList`/`glNewList` с именем `>= 1024` — OOB-запись).
    ListUndefined { list: u32 },
    /// `glNewList` при уже открытой компиляции (upstream — `assert`).
    ListAlreadyCompiling,
    /// `glEndList` без `glNewList` (upstream — `assert`).
    ListNotCompiling,
    /// `glEndList` при незакрытом `glBegin` внутри списка: сначала `end()`.
    ListBeginOpen { list: u32 },
    /// В списке осталось то, что на реплее упало бы (вершина вне `glBegin`,
    /// несбалансированный begin): список определён, но не вызываем.
    ListMalformed { list: u32 },
    /// Вызов списка при открытом `glBegin` живого контекста начал бы
    /// компиляцию/вызов с чужим `in_begin` — отвергаем.
    ListDuringBegin,
    /// Операция с клиентским указателем (`glTexImage2D`, `glGenTextures`,
    /// `glDeleteTextures`, `glGenLists`) внутри компиляции списка: C записал
    /// бы сырой указатель и на реплее прочитал освобождённую память.
    ListClientPointer,
    /// Список (прямо или через вложенные) вызывает сам себя: upstream
    /// переполнил бы C-стек.
    ListCycle { list: u32 },
}

impl fmt::Display for FbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FbError::WidthNotMultipleOfFour { width } => write!(
                f,
                "surface width {width} must be a multiple of 4 (TinyGL truncates xsize & ~3)"
            ),
            FbError::BadDimensions { width, height } => write!(
                f,
                "surface dimensions {width}x{height} must be in 1..={MAX_SURFACE_DIM}"
            ),
            FbError::BufferLenMismatch { expected, actual } => write!(
                f,
                "surface buffer has {actual} pixels, expected exactly {expected} (stride must be width*4)"
            ),
            FbError::ResizeTooSmall {
                required,
                available,
            } => write!(
                f,
                "resize needs {required} pixels but the existing buffer holds {available}; \
                 grow via a new Surface/Context (new memfd), not ZB_resize"
            ),
            FbError::BadViewport { width, height } => write!(
                f,
                "viewport {width}x{height} must be in 1..={MAX_SURFACE_DIM} (upstream glViewport exits on <= 0)"
            ),
            FbError::MatrixStackOverflow { mode } => {
                write!(f, "{mode} matrix stack is full; pop before push")
            }
            FbError::MatrixStackUnderflow { mode } => {
                write!(f, "{mode} matrix stack is empty; push before pop")
            }
            FbError::NonFinite { what } => {
                write!(f, "{what} contains NaN or infinity (C float→int cast is UB)")
            }
            FbError::BadProjection { reason } => write!(f, "invalid frustum: {reason}"),
            FbError::BadPerspective { reason } => write!(f, "invalid perspective: {reason}"),
            FbError::BeginNested => write!(f, "glBegin is already open; glEnd it first"),
            FbError::EndWithoutBegin => write!(f, "glEnd without a matching glBegin"),
            FbError::OpenFailed => write!(f, "ZB_open failed (could not allocate Z buffer)"),
            FbError::ContextAlreadyLive => write!(
                f,
                "another TinyGL Context is alive in this process (gl_ctx is global, no Send/Sync)"
            ),
            FbError::BadTextureCount { count } => write!(
                f,
                "texture count {count} must be in 1..=1024 (glGenTextures(0) does nothing)"
            ),
            FbError::BadTextureName { name } => write!(
                f,
                "texture name {name} does not fit TinyGL's signed int handle"
            ),
            FbError::BadTextureSize { width, height } => write!(
                f,
                "texture size {width}x{height} must be in 1..=4096 (upstream resamples to 256x256)"
            ),
            FbError::TextureDataLenMismatch { expected, actual } => write!(
                f,
                "RGB texture data has {actual} bytes, expected exactly {expected} (dense w*h*3 rows)"
            ),
            FbError::TextureNotBound => write!(
                f,
                "no texture is bound; call bind_texture first (upstream current_texture is NULL)"
            ),
            FbError::TextureHasNoImage { texture } => write!(
                f,
                "texture {texture} has no uploaded image; call tex_image_2d before enabling GL_TEXTURE_2D"
            ),
            FbError::TextureBoundAndEnabled { texture } => write!(
                f,
                "texture {texture} is bound and GL_TEXTURE_2D is enabled; \
                 disable the texture or bind another one before deleting it"
            ),
            FbError::TextureNameExhausted => write!(
                f,
                "generating more texture names would overflow TinyGL's int handle space"
            ),
            FbError::UnsupportedTextureParameter { pname, param } => write!(
                f,
                "glTexParameteri({pname}, {param}) is not supported upstream \
                 (wrap is always REPEAT, filtering is always NEAREST)"
            ),
            FbError::BadListRange { range } => write!(
                f,
                "display-list range {range} must be in 1..=1024 (upstream has 1024 slots)"
            ),
            FbError::ListSpaceExhausted => write!(
                f,
                "no run of free display-list names of the requested length remains"
            ),
            FbError::ListStateDiverged { expected, actual } => write!(
                f,
                "display-list occupancy mirror diverged from C (expected name {expected}, C returned {actual})"
            ),
            FbError::ListUndefined { list } => write!(
                f,
                "display list {list} is not defined (upstream glCallList would exit(1))"
            ),
            FbError::ListAlreadyCompiling => {
                write!(f, "a display list is already being compiled; call end_list first")
            }
            FbError::ListNotCompiling => write!(f, "glEndList without an open glNewList"),
            FbError::ListBeginOpen { list } => write!(
                f,
                "display list {list} still has an open glBegin; call end before end_list"
            ),
            FbError::ListMalformed { list } => write!(
                f,
                "display list {list} contains an operation that would abort upstream on replay \
                 (stray vertex or unbalanced begin); redefine or delete the list"
            ),
            FbError::ListDuringBegin => write!(
                f,
                "cannot open a display list while glBegin is open (upstream in_begin state)"
            ),
            FbError::ListClientPointer => write!(
                f,
                "client-pointer operation is not allowed during display-list compilation \
                 (C would replay a stale Rust pointer); do it outside glNewList/glEndList"
            ),
            FbError::ListCycle { list } => write!(
                f,
                "display list {list} is called recursively; upstream would overflow the C stack"
            ),
        }
    }
}

impl std::error::Error for FbError {}

/// Плотная пиксельная поверхность `0x00RRGGBB` (XRGB8888), принадлежащая
/// вызывающему. `Context` держит `&mut` на неё, пока жив, поэтому перевыделить
/// буфер под ним безопасно нельзя.
pub struct Surface<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
}

impl<'a> Surface<'a> {
    /// Проверить буфер и размеры. `pixels.len()` обязан быть ровно `w*h`.
    pub fn new(pixels: &'a mut [u32], width: u32, height: u32) -> Result<Self, FbError> {
        validate_dimensions(width, height)?;
        let expected = (width as usize) * (height as usize);
        if pixels.len() != expected {
            return Err(FbError::BufferLenMismatch {
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Surface {
            pixels,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Плотный stride в байтах — всегда `width * 4`.
    pub fn stride_bytes(&self) -> usize {
        (self.width as usize) * 4
    }

    /// Видимая площадь в пикселях. После `resize` меньше исходной ёмкости
    /// `pixels`: хвост буфера остаётся у вызывающего, но GL его не видит.
    pub fn area(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }

    /// Пиксели видимой области (ровно `width * height`).
    pub fn pixels(&self) -> &[u32] {
        &self.pixels[..self.area()]
    }

    /// Изменяемая видимая область.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let area = self.area();
        &mut self.pixels[..area]
    }

    /// Проверить размеры для [`Context::resize`](crate::Context::resize), ничего
    /// не меняя. Площадь не может превышать исходную ёмкость буфера.
    pub(crate) fn check_resize(&self, width: u32, height: u32) -> Result<(), FbError> {
        validate_dimensions(width, height)?;
        let required = (width as usize) * (height as usize);
        if required > self.pixels.len() {
            return Err(FbError::ResizeTooSmall {
                required,
                available: self.pixels.len(),
            });
        }
        Ok(())
    }

    /// Применить уже проверенный `check_resize`.
    pub(crate) fn apply_resize(&mut self, width: u32, height: u32) {
        debug_assert!((width as usize) * (height as usize) <= self.pixels.len());
        self.width = width;
        self.height = height;
    }

    /// Указатель для `ZB_open`/`ZB_resize`; C пишет в этот буфер, пока
    /// `Surface` жив.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut u32 {
        self.pixels.as_mut_ptr()
    }
}

/// Общая проверка размеров: `1..=MAX_SURFACE_DIM`, ширина кратна 4.
fn validate_dimensions(width: u32, height: u32) -> Result<(), FbError> {
    if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
        return Err(FbError::BadDimensions { width, height });
    }
    if !width.is_multiple_of(4) {
        return Err(FbError::WidthNotMultipleOfFour { width });
    }
    Ok(())
}

impl fmt::Debug for Surface<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Surface")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride_bytes", &self.stride_bytes())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_width_not_multiple_of_four() {
        let mut fb = vec![0u32; 65 * 64];
        assert_eq!(
            Surface::new(&mut fb, 65, 64).unwrap_err(),
            FbError::WidthNotMultipleOfFour { width: 65 }
        );
    }

    #[test]
    fn rejects_zero_and_huge_dimensions() {
        let mut fb = vec![0u32; 0];
        assert_eq!(
            Surface::new(&mut fb, 0, 64).unwrap_err(),
            FbError::BadDimensions {
                width: 0,
                height: 64
            }
        );
        let mut fb = vec![0u32; 4 * 4];
        assert_eq!(
            Surface::new(&mut fb, MAX_SURFACE_DIM + 4, 4).unwrap_err(),
            FbError::BadDimensions {
                width: MAX_SURFACE_DIM + 4,
                height: 4
            }
        );
    }

    #[test]
    fn rejects_small_buffer() {
        let mut fb = vec![0u32; 64 * 63];
        assert_eq!(
            Surface::new(&mut fb, 64, 64).unwrap_err(),
            FbError::BufferLenMismatch {
                expected: 64 * 64,
                actual: 64 * 63
            }
        );
    }

    #[test]
    fn rejects_padded_stride() {
        // PDP/1 разрешает stride > w*4, TinyGL — нет: лишние пиксели в строке
        // он не увидит, поэтому такой буфер отвергается, а не «ужимается».
        let mut fb = vec![0u32; 64 * 64 + 64];
        assert_eq!(
            Surface::new(&mut fb, 64, 64).unwrap_err(),
            FbError::BufferLenMismatch {
                expected: 64 * 64,
                actual: 64 * 64 + 64
            }
        );
    }

    #[test]
    fn accepts_dense_buffer() {
        let mut fb = vec![0u32; 64 * 64];
        let surface = Surface::new(&mut fb, 64, 64).expect("dense 64x64");
        assert_eq!(surface.stride_bytes(), 256);
        assert_eq!(surface.area(), 64 * 64);
    }

    #[test]
    fn resize_stays_within_capacity() {
        let mut fb = vec![0u32; 64 * 64];
        let mut surface = Surface::new(&mut fb, 64, 64).expect("dense 64x64");
        assert_eq!(surface.check_resize(32, 64), Ok(()));
        surface.apply_resize(32, 64);
        assert_eq!(surface.width(), 32);
        assert_eq!(surface.pixels().len(), 32 * 64);

        assert_eq!(
            surface.check_resize(128, 64),
            Err(FbError::ResizeTooSmall {
                required: 128 * 64,
                available: 64 * 64
            })
        );
        assert_eq!(
            surface.check_resize(33, 16),
            Err(FbError::WidthNotMultipleOfFour { width: 33 })
        );
    }
}
