//! PR2: безопасный FFI-контекст, матрицы/viewport, depth и culling.
//!
//! Тесты, создающие `Context`, сериализуются мьютексом: `gl_ctx` в TinyGL
//! глобальный, а cargo запускает тесты одного бинаря параллельно.
//!
//! Эталонный дайджест (`criterion 6`, спека §5.2) проверяется здесь на кадре
//! `angle = 0`: `0x3f7e5ece33386f9b`, центр `0x003c4c7a`. Для повёрнутых
//! кадров внешнего эталона нет — проверяется детерминизм (два прогона дают
//! один дайджест/пиксели), а не «записанное и принятое» значение.

use std::sync::{Mutex, MutexGuard};

use tinygl::{
    pixel_digest, Capability, Context, CullFace, FbError, FrontFace, GlError, MatrixMode,
    Primitive, Surface, Viewport, MAX_SURFACE_DIM,
};

const W: u32 = 64;
const H: u32 = 64;
/// Фон кадра: не совпадает ни с одним цветом сцены, поэтому culling/clear
/// проверяются точным сравнением с этим значением.
const CLEAR: u32 = 0x0010_2030;
const REFERENCE_DIGEST: u64 = 0x3f7e_5ece_3338_6f9b;
const REFERENCE_CENTER: u32 = 0x003c_4c7a;

static GL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn gl_test_lock() -> MutexGuard<'static, ()> {
    // Отравленный мьютекс не должен маскировать исходный assert_failure.
    GL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Frame {
    digest: u64,
    pixels: Vec<u32>,
}

impl Frame {
    fn center(&self) -> u32 {
        self.pixels[(32 * W + 32) as usize]
    }

    fn corner(&self) -> u32 {
        self.pixels[0]
    }
}

/// Полный headless-кадр: `Surface<Vec<u32>>` → `Context` → `clear` → сцена.
fn frame(clear_color: u32, draw: impl FnOnce(&Context)) -> Frame {
    let mut fb = vec![0u32; (W * H) as usize];
    {
        let surface = Surface::new(&mut fb, W, H).expect("64x64 is a valid surface");
        let ctx = Context::new(surface).expect("single live context");
        ctx.set_viewport(0, 0, W, H).expect("valid viewport");
        ctx.clear(clear_color, 0);
        draw(&ctx);
    }
    Frame {
        digest: pixel_digest(&fb),
        pixels: fb,
    }
}

/// Проекция эталонной пробы: `frustum(-1,1,-1,1,1,10)`, modelview
/// `translate(0,0,-3)`.
fn reference_projection(ctx: &Context) {
    ctx.matrix_mode(MatrixMode::Projection);
    ctx.load_identity();
    ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
        .expect("reference frustum");
    ctx.matrix_mode(MatrixMode::ModelView);
    ctx.load_identity();
    ctx.translate(0.0, 0.0, -3.0).expect("finite translate");
}

/// Треугольник эталонной пробы (RGB-углы), порядок обхода — CCW.
fn draw_reference_triangle(ctx: &Context) {
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.color3f(1.0, 0.0, 0.0);
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.color3f(0.0, 1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.color3f(0.0, 0.0, 1.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("end");
}

/// Треугольник одного цвета; `ccw == false` меняет порядок обхода на CW.
fn draw_uniform_triangle(ctx: &Context, ccw: bool) {
    let (a, b, c) = (
        (-1.0f32, -1.0f32, 0.0f32),
        (1.0, -1.0, 0.0),
        (0.0, 1.0, 0.0),
    );
    let order = if ccw { [a, b, c] } else { [a, c, b] };
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.color3f(1.0, 1.0, 0.0);
    for vertex in order {
        ctx.vertex3f(vertex.0, vertex.1, vertex.2);
    }
    ctx.end().expect("end");
}

fn render_reference_rotated(angle_degrees: f32) -> Frame {
    frame(0x0000_0000, |ctx| {
        reference_projection(ctx);
        ctx.rotate(angle_degrees, 0.0, 0.0, 1.0)
            .expect("finite rotate");
        draw_reference_triangle(ctx);
    })
}

fn render_cull(cull: Option<(CullFace, FrontFace)>, ccw: bool) -> Frame {
    frame(CLEAR, |ctx| {
        reference_projection(ctx);
        if let Some((face, winding)) = cull {
            ctx.set_front_face(winding);
            ctx.set_cull_face(face);
            ctx.enable(Capability::CullFace);
        }
        draw_uniform_triangle(ctx, ccw);
    })
}

fn draw_depth_triangle(ctx: &Context, z: f32, rgb: (f32, f32, f32)) {
    ctx.matrix_mode(MatrixMode::ModelView);
    ctx.load_identity();
    ctx.translate(0.0, 0.0, z).expect("finite translate");
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.color3f(rgb.0, rgb.1, rgb.2);
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("end");
}

fn render_depth(order: &[f32], depth_test: bool) -> Frame {
    frame(0x0000_0000, |ctx| {
        ctx.matrix_mode(MatrixMode::Projection);
        ctx.load_identity();
        ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            .expect("frustum");
        if depth_test {
            ctx.enable(Capability::DepthTest);
        }
        for z in order {
            let rgb = if *z == -2.0 {
                (1.0, 0.0, 0.0)
            } else {
                (0.0, 0.0, 1.0)
            };
            draw_depth_triangle(ctx, *z, rgb);
        }
    })
}

// --- triangle / digest -----------------------------------------------------

#[test]
fn reference_frame_still_matches_criterion6() {
    let _guard = gl_test_lock();
    let reference = render_reference_rotated(0.0);
    assert_eq!(
        reference.digest, REFERENCE_DIGEST,
        "frame 0 must stay the criterion-6 reference (block until classified)"
    );
    assert_eq!(reference.center(), REFERENCE_CENTER);
    assert_eq!(reference.corner(), 0);
}

#[test]
fn triangle_frames_are_deterministic() {
    let _guard = gl_test_lock();
    let mut digests = Vec::new();
    for frame_index in 0..6u32 {
        let angle = frame_index as f32 * 30.0;
        let first = render_reference_rotated(angle);
        let second = render_reference_rotated(angle);
        assert_eq!(
            first.digest, second.digest,
            "frame {frame_index} (angle {angle}) must hash identically on two runs"
        );
        assert_eq!(
            first.pixels, second.pixels,
            "frame {frame_index} (angle {angle}) must be pixel-identical on two runs"
        );
        digests.push(first.digest);
    }
    assert_eq!(digests[0], REFERENCE_DIGEST, "angle 0 is the reference");
    assert!(
        digests.windows(2).any(|pair| pair[0] != pair[1]),
        "rotated frames must not all collapse to one digest: {digests:x?}"
    );
}

#[test]
fn geometry_outside_the_frustum_never_touches_the_buffer() {
    let _guard = gl_test_lock();
    // Пустой кадр — эталон «ничего не нарисовано».
    let empty = frame(CLEAR, |_ctx| {});
    // Треугольник целиком за пределами frustum: GL обязан его отсечь, а наш
    // буфер — остаться побайтово равным пустому кадру (clip не пишет мимо).
    let offscreen = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        ctx.translate(100.0, 0.0, 0.0).expect("finite translate");
        draw_reference_triangle(ctx);
    });
    assert_eq!(offscreen.digest, empty.digest);
    assert_eq!(offscreen.pixels, empty.pixels);
}

// --- depth ----------------------------------------------------------------

#[test]
fn depth_near_triangle_wins_regardless_of_draw_order() {
    let _guard = gl_test_lock();
    // Одиночные треугольники дают «эталонные» цвета без магических констант.
    let near_only = render_depth(&[-2.0], false);
    let far_only = render_depth(&[-4.0], false);
    let near_color = near_only.center();
    assert_ne!(
        near_color,
        far_only.center(),
        "near and far triangles must produce different colours"
    );
    assert_ne!(near_color, 0, "near triangle must cover the centre");

    for (label, order) in [
        ("near then far", [-2.0, -4.0]),
        ("far then near", [-4.0, -2.0]),
    ] {
        let rendered = render_depth(&order, false);
        assert_eq!(
            rendered.center(),
            near_color,
            "{label}: upstream Z buffer keeps the nearer triangle"
        );
    }

    // GL_DEPTH_TEST у 0.4.1 переключает Z только у линий; заливка тестируется
    // всегда — поэтому включение cap ничего не меняет для треугольников.
    for order in [[-2.0, -4.0], [-4.0, -2.0]] {
        let rendered = render_depth(&order, true);
        assert_eq!(
            rendered.center(),
            near_color,
            "GL_DEPTH_TEST must not change filled-triangle depth behaviour (upstream)"
        );
    }
}

// --- culling --------------------------------------------------------------

#[test]
fn culling_honours_winding_and_front_face() {
    let _guard = gl_test_lock();
    // Без culling рисуются оба обхода.
    assert_ne!(render_cull(None, true).center(), CLEAR);
    assert_ne!(render_cull(None, false).center(), CLEAR);

    // По умолчанию front=CCW, cull=BACK.
    let ccw_back = render_cull(Some((CullFace::Back, FrontFace::Ccw)), true);
    let cw_back = render_cull(Some((CullFace::Back, FrontFace::Ccw)), false);
    assert_ne!(ccw_back.center(), CLEAR, "CCW must be front-facing");
    assert_eq!(cw_back.center(), CLEAR, "CW must be culled as back-facing");

    // Front-face CW переворачивает, кто лицевой.
    let ccw_cw = render_cull(Some((CullFace::Back, FrontFace::Cw)), true);
    let cw_cw = render_cull(Some((CullFace::Back, FrontFace::Cw)), false);
    assert_eq!(ccw_cw.center(), CLEAR, "CCW is back-facing when front=CW");
    assert_ne!(cw_cw.center(), CLEAR, "CW is front-facing when front=CW");

    // Cull FRONT отбрасывает лицевые, оставляя задние.
    assert_eq!(
        render_cull(Some((CullFace::Front, FrontFace::Ccw)), true).center(),
        CLEAR
    );
    assert_ne!(
        render_cull(Some((CullFace::Front, FrontFace::Ccw)), false).center(),
        CLEAR
    );

    // FRONT_AND_BACK у upstream отбрасывает всё.
    assert_eq!(
        render_cull(Some((CullFace::FrontAndBack, FrontFace::Ccw)), true).center(),
        CLEAR
    );
    assert_eq!(
        render_cull(Some((CullFace::FrontAndBack, FrontFace::Ccw)), false).center(),
        CLEAR
    );
}

// --- matrices / viewport --------------------------------------------------

fn assert_matrix_close(actual: &[f32; 16], expected: &[f64; 16]) {
    for index in 0..16 {
        let delta = (f64::from(actual[index]) - expected[index]).abs();
        assert!(
            delta < 1e-5,
            "matrix element {index}: actual={} expected={} delta={delta}",
            actual[index],
            expected[index]
        );
    }
}

#[test]
fn matrices_roundtrip_through_c() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; (W * H) as usize];
    let surface = Surface::new(&mut fb, W, H).expect("64x64");
    let ctx = Context::new(surface).expect("single context");

    // glInit оставляет modelview единичной.
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::ModelView),
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ],
    );

    ctx.translate(1.0, 2.0, 3.0).expect("finite translate");
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::ModelView),
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 2.0, 3.0, 1.0,
        ],
    );

    // push/pop: translate(10,0,0) поверх (1,2,3) → x=11, после pop — снова 1.
    ctx.push_matrix().expect("push");
    ctx.translate(10.0, 0.0, 0.0).expect("translate");
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::ModelView),
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 11.0, 2.0, 3.0, 1.0,
        ],
    );
    ctx.pop_matrix().expect("pop");
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::ModelView),
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 2.0, 3.0, 1.0,
        ],
    );

    // glRotatef(90°, z): x→y, y→-x (column-major).
    ctx.load_identity();
    ctx.rotate(90.0, 0.0, 0.0, 1.0).expect("rotate");
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::ModelView),
        &[
            0.0, 1.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ],
    );
    ctx.load_identity();

    // load_matrix/mult_matrix принимают ту же column-major раскладку.
    let translate_four = [
        1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 4.0, 5.0, 6.0, 1.0,
    ];
    ctx.load_matrix(&translate_four).expect("load matrix");
    assert_eq!(ctx.get_matrix(MatrixMode::ModelView), translate_four);
    ctx.load_identity();
    ctx.mult_matrix(&translate_four).expect("mult matrix");
    assert_eq!(ctx.get_matrix(MatrixMode::ModelView), translate_four);

    // glFrustum(±1, ±1, 1, 10) — проверяем формулу из matrix.c.
    ctx.matrix_mode(MatrixMode::Projection);
    ctx.load_identity();
    ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
        .expect("frustum");
    let near = 1.0f64;
    let far = 10.0f64;
    let c = -(far + near) / (far - near);
    let d = -(2.0 * far * near) / (far - near);
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::Projection),
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, c, -1.0, 0.0, 0.0, d, 0.0,
        ],
    );

    // perspective() — наш gluPerspective-эквивалент: та же формула.
    ctx.load_identity();
    ctx.perspective(60.0, 1.5, 2.0, 20.0).expect("perspective");
    let near = 2.0f64;
    let far = 20.0f64;
    let top = near * (std::f64::consts::PI / 6.0).tan();
    let right = top * 1.5;
    let x = 2.0 * near / (right + right);
    let y = 2.0 * near / (top + top);
    let c = -(far + near) / (far - near);
    let d = -(2.0 * far * near) / (far - near);
    assert_matrix_close(
        &ctx.get_matrix(MatrixMode::Projection),
        &[
            x, 0.0, 0.0, 0.0, 0.0, y, 0.0, 0.0, 0.0, 0.0, c, -1.0, 0.0, 0.0, d, 0.0,
        ],
    );
}

#[test]
fn viewport_readback_and_rejection() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; (W * H) as usize];
    let surface = Surface::new(&mut fb, W, H).expect("64x64");
    let ctx = Context::new(surface).expect("single context");

    ctx.set_viewport(3, 5, 32, 16).expect("valid viewport");
    assert_eq!(
        ctx.viewport(),
        Viewport {
            x: 3,
            y: 5,
            width: 32,
            height: 16
        }
    );

    // Нулевой/огромный viewport — наш Err, а не upstream exit(1); состояние
    // остаётся прежним.
    assert_eq!(
        ctx.set_viewport(0, 0, 0, 16),
        Err(FbError::BadViewport {
            width: 0,
            height: 16
        })
    );
    assert_eq!(
        ctx.set_viewport(0, 0, MAX_SURFACE_DIM + 4, 16),
        Err(FbError::BadViewport {
            width: MAX_SURFACE_DIM + 4,
            height: 16
        })
    );
    assert_eq!(ctx.viewport().width, 32);
    assert_eq!(ctx.viewport().height, 16);
}

// --- resize ---------------------------------------------------------------

#[test]
fn resize_reconfigures_zbuffer_within_buffer_capacity() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; (W * H) as usize];
    {
        let surface = Surface::new(&mut fb, W, H).expect("64x64");
        let mut ctx = Context::new(surface).expect("single context");
        ctx.set_viewport(0, 0, W, H).expect("64x64 viewport");

        ctx.resize(32, 64).expect("shrink to 32x64 fits");
        assert_eq!(ctx.surface().width(), 32);
        assert_eq!(ctx.surface().height(), 64);
        assert_eq!(ctx.pixels().len(), 32 * 64);
        assert_eq!(
            ctx.viewport(),
            Viewport {
                x: 0,
                y: 0,
                width: 32,
                height: 64
            }
        );
        ctx.clear(0x00ab_cdef, 0);
        assert!(
            ctx.pixels().iter().all(|pixel| *pixel == 0x00ab_cdef),
            "clear after ZB_resize must fill exactly the visible area"
        );

        // Обратно в исходный размер — площадь не больше ёмкости.
        ctx.resize(64, 64).expect("grow back to original capacity");
        assert_eq!(ctx.pixels().len(), 64 * 64);

        // Рост за ёмкость — наш Err, ZBuffer не трогаем.
        assert_eq!(
            ctx.resize(128, 64),
            Err(FbError::ResizeTooSmall {
                required: 128 * 64,
                available: 64 * 64
            })
        );
        assert_eq!(
            ctx.resize(33, 16),
            Err(FbError::WidthNotMultipleOfFour { width: 33 })
        );
        assert_eq!(
            ctx.resize(0, 16),
            Err(FbError::BadDimensions {
                width: 0,
                height: 16
            })
        );
        assert_eq!(ctx.surface().width(), 64);
        assert_eq!(ctx.viewport().width, 64);
    }
    // Context закрыт; буфер снова доступен целиком.
    assert_eq!(fb.len(), (W * H) as usize);
}

// --- bad inputs / state guards -------------------------------------------

#[test]
fn bad_inputs_are_errors_before_c() {
    let _guard = gl_test_lock();
    // Surface: всё то же, что проверяет unit-тест surface.rs, но здесь важен
    // контракт «Context не открывается на плохом входе».
    let mut zero = vec![0u32; 0];
    assert_eq!(
        Surface::new(&mut zero, 0, 64).err(),
        Some(FbError::BadDimensions {
            width: 0,
            height: 64
        })
    );
    let mut not_multiple = vec![0u32; 65 * 64];
    assert_eq!(
        Surface::new(&mut not_multiple, 65, 64).err(),
        Some(FbError::WidthNotMultipleOfFour { width: 65 })
    );
    let mut small = vec![0u32; 64 * 63];
    assert_eq!(
        Surface::new(&mut small, 64, 64).err(),
        Some(FbError::BufferLenMismatch {
            expected: 64 * 64,
            actual: 64 * 63
        })
    );
    let mut padded = vec![0u32; 64 * 64 + 1];
    assert_eq!(
        Surface::new(&mut padded, 64, 64).err(),
        Some(FbError::BufferLenMismatch {
            expected: 64 * 64,
            actual: 64 * 64 + 1
        })
    );

    let mut first_fb = vec![0u32; 16 * 16];
    let mut second_fb = vec![0u32; 16 * 16];
    let first_surface = Surface::new(&mut first_fb, 16, 16).expect("16x16");
    let first = Context::new(first_surface).expect("first context");
    let second_surface = Surface::new(&mut second_fb, 16, 16).expect("16x16");
    assert_eq!(
        Context::new(second_surface).err(),
        Some(FbError::ContextAlreadyLive)
    );

    // state guards: end без begin, вложенный begin, NaN/∞, вырожденные
    // проекции — всё наши Err, а upstream assert/UB не достигается.
    assert_eq!(first.end(), Err(FbError::EndWithoutBegin));
    first.begin(Primitive::Triangles).expect("begin");
    assert_eq!(first.begin(Primitive::Lines), Err(FbError::BeginNested));
    first.end().expect("end");
    assert_eq!(first.end(), Err(FbError::EndWithoutBegin));

    assert_eq!(
        first.translate(f32::NAN, 0.0, 0.0),
        Err(FbError::NonFinite { what: "translate" })
    );
    assert_eq!(
        first.rotate(0.0, f32::INFINITY, 0.0, 0.0),
        Err(FbError::NonFinite { what: "rotate" })
    );
    assert_eq!(
        first.scale(1.0, f32::NAN, 1.0),
        Err(FbError::NonFinite { what: "scale" })
    );
    let mut bad_matrix = [0.0f32; 16];
    bad_matrix[3] = f32::NAN;
    assert_eq!(
        first.load_matrix(&bad_matrix),
        Err(FbError::NonFinite { what: "matrix" })
    );
    assert_eq!(
        first.mult_matrix(&bad_matrix),
        Err(FbError::NonFinite { what: "matrix" })
    );

    assert!(matches!(
        first.frustum(1.0, 1.0, -1.0, 1.0, 1.0, 10.0),
        Err(FbError::BadProjection { .. })
    ));
    assert!(matches!(
        first.frustum(-1.0, 1.0, 0.0, 0.0, 1.0, 10.0),
        Err(FbError::BadProjection { .. })
    ));
    assert!(matches!(
        first.frustum(-1.0, 1.0, -1.0, 1.0, 0.0, 10.0),
        Err(FbError::BadProjection { .. })
    ));
    assert!(matches!(
        first.frustum(-1.0, 1.0, -1.0, 1.0, 10.0, 5.0),
        Err(FbError::BadProjection { .. })
    ));
    assert!(matches!(
        first.perspective(0.0, 1.0, 1.0, 10.0),
        Err(FbError::BadPerspective { .. })
    ));
    assert!(matches!(
        first.perspective(60.0, 0.0, 1.0, 10.0),
        Err(FbError::BadPerspective { .. })
    ));
    assert!(matches!(
        first.perspective(60.0, 1.0, 0.0, 10.0),
        Err(FbError::BadPerspective { .. })
    ));
    assert!(matches!(
        first.perspective(60.0, 1.0, 10.0, 10.0),
        Err(FbError::BadPerspective { .. })
    ));
    assert_eq!(first.get_error(), GlError::NoError);

    // После всех отказов контекст по-прежнему работоспособен.
    first.set_viewport(0, 0, 16, 16).expect("valid viewport");
    first.clear(0x0000_0000, 0);
    first.end().ok();
}

#[test]
fn matrix_stacks_are_guarded_not_asserted() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");

    let limit = ctx.max_modelview_stack_depth();
    assert!(limit >= 2, "upstream MAX_MODELVIEW_STACK_DEPTH");
    for _ in 0..(limit - 1) {
        ctx.push_matrix().expect("push within limit");
    }
    assert_eq!(
        ctx.push_matrix(),
        Err(FbError::MatrixStackOverflow { mode: "modelview" })
    );
    for _ in 0..(limit - 1) {
        ctx.pop_matrix().expect("pop within depth");
    }
    assert_eq!(
        ctx.pop_matrix(),
        Err(FbError::MatrixStackUnderflow { mode: "modelview" })
    );

    ctx.matrix_mode(MatrixMode::Projection);
    let limit = ctx.max_projection_stack_depth();
    assert!(limit >= 2);
    for _ in 0..(limit - 1) {
        ctx.push_matrix().expect("push projection");
    }
    assert_eq!(
        ctx.push_matrix(),
        Err(FbError::MatrixStackOverflow { mode: "projection" })
    );
    for _ in 0..(limit - 1) {
        ctx.pop_matrix().expect("pop projection");
    }
    assert_eq!(
        ctx.pop_matrix(),
        Err(FbError::MatrixStackUnderflow { mode: "projection" })
    );

    // Заявленные upstream константы видны через glGetIntegerv-обёртку.
    assert_eq!(ctx.max_lights(), 16);
    assert_eq!(ctx.max_texture_size(), 256);
}

#[test]
fn drop_is_clean_and_reopenable() {
    let _guard = gl_test_lock();
    for _ in 0..8 {
        let mut fb = vec![0u32; 16 * 16];
        let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
        let ctx = Context::new(surface).expect("sequential contexts open");
        ctx.set_viewport(0, 0, 16, 16).expect("viewport");
        ctx.clear(0x0000_0000, 0);
        drop(ctx);
    }
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    Context::new(surface).expect("context opens after all drops");
}
