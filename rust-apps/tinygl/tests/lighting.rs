//! PR5: минимальные тесты освещения — `glLightfv`/`glMaterialfv`, которые
//! нужны демо `gears` (fixed-function lighting upstream 0.4.1).
//!
//! Проверяем **поведение**, а не внутренности C: включённый `GL_LIGHTING` +
//! `GL_LIGHT0` меняет цвет заливки, материал задаёт оттенок, а NaN-параметр
//! отвергается нашим слоем до `exit(1)`/NaN-кадра.
//!
//! `gl_ctx` у TinyGL глобальный, поэтому все тесты идут под одним мьютексом.

use std::sync::{Mutex, MutexGuard};

use tinygl::{
    pixel_digest, Capability, Context, FbError, Light, LightParam, MaterialFace, MaterialParam,
    MatrixMode, Primitive, Surface,
};

static GL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn gl_test_lock() -> MutexGuard<'static, ()> {
    GL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

const W: u32 = 64;
const H: u32 = 64;

/// Квадрат на весь кадр, нормаль к камере (+Z), плоская заливка.
fn render_quad(lighting: bool, material: [f32; 4]) -> (u64, u32) {
    let mut fb = vec![0u32; (W * H) as usize];
    let digest;
    let center;
    {
        let surface = Surface::new(&mut fb, W, H).expect("64x64");
        let ctx = Context::new(surface).expect("single context");
        ctx.set_viewport(0, 0, W, H).expect("viewport");
        ctx.clear(0x0000_0000, 0);
        ctx.matrix_mode(MatrixMode::Projection);
        ctx.load_identity();
        ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            .expect("frustum");
        ctx.matrix_mode(MatrixMode::ModelView);
        ctx.load_identity();
        ctx.translate(0.0, 0.0, -3.0).expect("translate");
        ctx.set_smooth_shading(false);
        if lighting {
            ctx.light(Light::Light0, LightParam::Position, [0.0, 0.0, 1.0, 0.0])
                .expect("light position");
            ctx.light(Light::Light0, LightParam::Diffuse, [1.0, 1.0, 1.0, 1.0])
                .expect("light diffuse");
            ctx.material(
                MaterialFace::FrontAndBack,
                MaterialParam::AmbientAndDiffuse,
                material,
            )
            .expect("material");
            ctx.enable(Capability::Lighting);
            ctx.enable(Capability::Light0);
        }
        ctx.begin(Primitive::Quads).expect("begin");
        if !lighting {
            ctx.color3f(0.0, 0.0, 1.0);
        }
        ctx.normal3f(0.0, 0.0, 1.0);
        ctx.vertex3f(-1.0, -1.0, 0.0);
        ctx.normal3f(0.0, 0.0, 1.0);
        ctx.vertex3f(1.0, -1.0, 0.0);
        ctx.normal3f(0.0, 0.0, 1.0);
        ctx.vertex3f(1.0, 1.0, 0.0);
        ctx.normal3f(0.0, 0.0, 1.0);
        ctx.vertex3f(-1.0, 1.0, 0.0);
        ctx.end().expect("end");
        digest = pixel_digest(ctx.pixels());
        center = ctx.pixels()[(32 * W + 32) as usize];
    }
    (digest, center)
}

#[test]
fn lighting_changes_the_fill_and_material_sets_the_colour() {
    let _guard = gl_test_lock();
    let (off_digest, off_center) = render_quad(false, [0.0, 0.0, 1.0, 1.0]);
    let (red_digest, red_center) = render_quad(true, [1.0, 0.0, 0.0, 1.0]);

    assert_ne!(off_digest, 0, "unlit quad must not be an empty buffer");
    assert_ne!(off_center, 0x0000_0000, "unlit quad must be blue");
    assert_ne!(
        red_digest, off_digest,
        "GL_LIGHTING must change the rendered pixels"
    );
    // TinyGL хранит цвет вершины в 16-битной сетке, поэтому каналы не
    // байт-точные: проверяем доминанту, а не 0x00ff0000.
    assert!(
        red_center >> 16 & 0xff >= 0xf0,
        "lit red material must be red-dominant; got 0x{red_center:08x}"
    );
    assert!(
        red_center & 0x0000_ffff <= 0x0000_2020,
        "lit red material must not leak green/blue; got 0x{red_center:08x}"
    );
}

#[test]
fn diffuse_material_is_visible_without_any_gl_color_call() {
    let _guard = gl_test_lock();
    let (_, center) = render_quad(true, [0.0, 1.0, 0.0, 1.0]);
    assert!(
        center >> 8 & 0xff >= 0xf0,
        "green material must reach the pixel with lighting on; got 0x{center:08x}"
    );
    assert!(
        center & 0x00ff_00ff <= 0x0020_0020,
        "green material must not leak red/blue; got 0x{center:08x}"
    );
}

#[test]
fn non_finite_light_and_material_are_rejected() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; (8 * 8) as usize];
    let surface = Surface::new(&mut fb, 8, 8).expect("8x8");
    let ctx = Context::new(surface).expect("single context");
    assert_eq!(
        ctx.light(
            Light::Light0,
            LightParam::Position,
            [f32::NAN, 0.0, 1.0, 0.0]
        ),
        Err(FbError::NonFinite {
            what: "light parameter"
        })
    );
    assert_eq!(
        ctx.material(
            MaterialFace::Front,
            MaterialParam::Diffuse,
            [0.0, f32::INFINITY, 0.0, 1.0]
        ),
        Err(FbError::NonFinite {
            what: "material parameter"
        })
    );
}
