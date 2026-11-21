//! PR1 criterion 6 (строгая): детерминированный треугольник из эталонной
//! пробы спеки `docs/tinygl-port-spec.md` §5.2 обязан дать
//! `digest=0x3f7e5ece33386f9b center=0x003c4c7a corner=0x00000000`.
//!
//! Любое расхождение — БЛОКИРОВКА PR1 до классификации (спека §5.2,
//! критерий 6): допустима только доказанная FP-reassociation при идентичной
//! геометрии; другой центр/площадь/цвет = FAIL. Поэтому тест сравнивает **и**
//! дайджест, **и** точечные пиксели.
//!
//! Тесты, работающие с `Context`, выполняются в одном тесте последовательно:
//! `gl_ctx` в TinyGL глобальный, и параллельные тесты подрались бы за него.

use std::sync::{Mutex, MutexGuard};

use tinygl::{pixel_digest, Context, FbError, MatrixMode, Primitive, Surface, MAX_SURFACE_DIM};

const W: u32 = 64;
const H: u32 = 64;

/// Тесты cargo идут параллельно, а `gl_ctx` у TinyGL один на процесс.
/// Сериализуем GL-тесты этим мьютексом (сам `Context` дополнительно вернёт
/// `ContextAlreadyLive`, если инвариант всё же нарушен).
static GL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn gl_test_lock() -> MutexGuard<'static, ()> {
    // Отравленный мьютекс не должен маскировать исходный assert_failure:
    // продолжаем с внутренним guard'ом.
    GL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Эталон пробы: `fb[64*64]`, залит `0xAB`, clear(0), frustum + translate,
/// RGB-треугольник. Возвращает `(digest, center, corner, fb)`.
fn render_reference_triangle() -> (u64, u32, u32, Vec<u32>) {
    let mut fb = vec![0xABu32; (W * H) as usize];
    let (digest, center, corner) = {
        let surface = Surface::new(&mut fb, W, H).expect("64x64 is a valid surface");
        let ctx = Context::new(surface).expect("no other context in this test");
        ctx.set_viewport(0, 0, W, H).expect("64x64 viewport");
        ctx.clear(0x0000_0000, 0);
        ctx.matrix_mode(MatrixMode::Projection);
        ctx.load_identity();
        ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            .expect("reference frustum");
        ctx.matrix_mode(MatrixMode::ModelView);
        ctx.load_identity();
        ctx.translate(0.0, 0.0, -3.0).expect("finite translate");
        ctx.begin(Primitive::Triangles).expect("begin");
        ctx.color3f(1.0, 0.0, 0.0);
        ctx.vertex3f(-1.0, -1.0, 0.0);
        ctx.color3f(0.0, 1.0, 0.0);
        ctx.vertex3f(1.0, -1.0, 0.0);
        ctx.color3f(0.0, 0.0, 1.0);
        ctx.vertex3f(0.0, 1.0, 0.0);
        ctx.end().expect("end");
        (
            pixel_digest(ctx.pixels()),
            ctx.pixels()[(32 * W + 32) as usize],
            ctx.pixels()[0],
        )
    };
    (digest, center, corner, fb)
}

#[test]
fn criterion6_triangle_digest_matches_reference() {
    let _guard = gl_test_lock();
    let (digest, center, corner, fb) = render_reference_triangle();

    assert_eq!(
        digest, 0x3f7e_5ece_3338_6f9b,
        "criterion 6 FAIL: digest diverged from the upstream reference \
         (block PR1 until classified per spec §5.2)"
    );
    assert_eq!(
        center, 0x003c_4c7a,
        "criterion 6 FAIL: center pixel diverged — geometry/color changed, not FP reassociation"
    );
    assert_eq!(corner, 0x0000_0000, "corner must stay the clear colour");
    assert_eq!(fb[0], 0, "corner pixel in the frame buffer must be clear");
    // Старший байт нулевой: 0x00RRGGBB (XRGB8888), как у композитора.
    assert_eq!(center >> 24, 0);
}

#[test]
fn criterion6_triangle_is_deterministic() {
    let _guard = gl_test_lock();
    let first = render_reference_triangle();
    let second = render_reference_triangle();
    assert_eq!(first.0, second.0, "two runs must produce one digest");
    assert_eq!(first.1, second.1);
    assert_eq!(first.3, second.3, "two runs must produce identical pixels");
}

#[test]
fn clear_is_uniform_and_keeps_xrgb() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; (W * H) as usize];
    {
        let surface = Surface::new(&mut fb, W, H).expect("64x64");
        let ctx = Context::new(surface).expect("single context");
        ctx.clear(0x003c_4c7a, 0);
    }
    assert!(
        fb.iter().all(|pixel| *pixel == 0x003c_4c7a),
        "glClear must fill every pixel with the requested 0x00RRGGBB"
    );
}

#[test]
fn context_is_single_and_reusable_after_drop() {
    let _guard = gl_test_lock();
    let mut first_fb = vec![0u32; 16 * 16];
    let mut second_fb = vec![0u32; 16 * 16];

    let first_surface = Surface::new(&mut first_fb, 16, 16).expect("16x16");
    let first = Context::new(first_surface).expect("first context");
    let second_surface = Surface::new(&mut second_fb, 16, 16).expect("16x16");
    assert_eq!(
        Context::new(second_surface).err(),
        Some(FbError::ContextAlreadyLive),
        "TinyGL has one global gl_ctx; a second live Context must be a clean error"
    );
    drop(first);

    // После Drop маркер свободен — новый контекст снова открывается.
    let mut third_fb = vec![0u32; 16 * 16];
    let third_surface = Surface::new(&mut third_fb, 16, 16).expect("16x16");
    Context::new(third_surface).expect("context opens again after drop");
}

#[test]
fn surface_validation_is_covered_here_too() {
    // Валидация подробно покрыта unit-тестами `surface.rs`; здесь — связка
    // «Context не открывается на плохом буфере» и предел композитора.
    let mut fb = vec![0u32; 64 * 64];
    let surface = Surface::new(&mut fb, 65, 64);
    assert_eq!(
        surface.err(),
        Some(FbError::WidthNotMultipleOfFour { width: 65 })
    );

    let mut huge = vec![0u32; 4 * 4];
    assert_eq!(
        Surface::new(&mut huge, MAX_SURFACE_DIM + 4, 4).err(),
        Some(FbError::BadDimensions {
            width: MAX_SURFACE_DIM + 4,
            height: 4
        })
    );
}
