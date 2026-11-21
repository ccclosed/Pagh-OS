//! PR3: текстуры (upstream-подмножество) и пиксельные проверки сцен.
//!
//! Эталонов «извне» для текстур нет: дайджесты сцен закреплены здесь после
//! первого детерминированного прогона (host + musl, см. `UPSTREAM.md`). Любое
//! расхождение дайджеста при неизменной геометрии (центр/покрытие) —
//! блокировка до классификации (FP/цвет), а расхождение геометрии — FAIL;
//! сравнение центра и покрытия отдельно не даёт «записать и принять» новый
//! кадр молча. Тесты с `Context` сериализуются: `gl_ctx` в TinyGL глобальный.

use std::sync::{Mutex, MutexGuard};

use tinygl::{
    pixel_digest, Context, FbError, MatrixMode, Primitive, Surface, TextureEnvMode, TextureParam,
    TexturePname,
};

const W: u32 = 64;
const H: u32 = 64;
/// Фон, не совпадающий ни с одним цветом текстуры/сцены.
const CLEAR: u32 = 0x0010_2030;

static GL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn gl_test_lock() -> MutexGuard<'static, ()> {
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

    fn coverage(&self) -> usize {
        self.pixels.iter().filter(|pixel| **pixel != CLEAR).count()
    }

    fn at(&self, x: u32, y: u32) -> u32 {
        self.pixels[(y * W + x) as usize]
    }
}

fn frame(clear_color: u32, draw: impl FnOnce(&Context<'_>)) -> Frame {
    let mut fb = vec![0u32; (W * H) as usize];
    {
        let surface = Surface::new(&mut fb, W, H).expect("64x64 surface");
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

/// Проекция текстурного quad'а: тот же `translate(0,0,-3)`, но frustum уже
/// (`±1/3`), чтобы quad `±1` занимал весь экран 64×64.
fn quad_projection(ctx: &Context<'_>) {
    ctx.matrix_mode(MatrixMode::Projection);
    ctx.load_identity();
    ctx.frustum(-1.0 / 3.0, 1.0 / 3.0, -1.0 / 3.0, 1.0 / 3.0, 1.0, 10.0)
        .expect("quad frustum");
    ctx.matrix_mode(MatrixMode::ModelView);
    ctx.load_identity();
    ctx.translate(0.0, 0.0, -3.0).expect("finite translate");
}

/// Проекция эталонной пробы PR1 (`frustum(-1,1,-1,1,1,10)`, `translate(0,0,-3)`).
fn reference_projection(ctx: &Context<'_>) {
    ctx.matrix_mode(MatrixMode::Projection);
    ctx.load_identity();
    ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
        .expect("reference frustum");
    ctx.matrix_mode(MatrixMode::ModelView);
    ctx.load_identity();
    ctx.translate(0.0, 0.0, -3.0).expect("finite translate");
}

/// 8×8 RGB-текстура из четырёх квадрантов (2×2 блока 4×4):
/// верх-лево красный, верх-право зелёный, низ-лево синий, низ-право белый.
/// Аплоад ресемплит её в 256×256 без интерполяции (квадранты 128×128).
fn quadrant_rgb() -> Vec<u8> {
    let mut data = Vec::with_capacity(8 * 8 * 3);
    for y in 0..8u32 {
        for x in 0..8u32 {
            let (r, g, b) = match (x >= 4, y >= 4) {
                (false, false) => (255, 0, 0),
                (true, false) => (0, 255, 0),
                (false, true) => (0, 0, 255),
                (true, true) => (255, 255, 255),
            };
            data.extend_from_slice(&[r, g, b]);
        }
    }
    data
}

/// Привязать новую текстуру, загрузить квадранты и включить `GL_TEXTURE_2D`.
fn bind_quadrant_texture(ctx: &Context<'_>) -> u32 {
    let names = ctx.gen_textures(1).expect("texture name");
    ctx.tex_env(TextureEnvMode::Decal);
    for (pname, param) in [
        (TexturePname::WrapS, TextureParam::Repeat),
        (TexturePname::WrapT, TextureParam::Repeat),
        (TexturePname::MinFilter, TextureParam::Nearest),
        (TexturePname::MagFilter, TextureParam::Nearest),
    ] {
        ctx.tex_parameter(pname, param)
            .expect("supported texture parameter");
    }
    ctx.bind_texture(names[0]).expect("bind texture");
    ctx.tex_image_2d(8, 8, &quadrant_rgb())
        .expect("upload RGB8");
    ctx.enable_texture_2d().expect("texture has an image");
    names[0]
}

/// Полноэкранный текстурный треугольник (`GL_TRIANGLES`, UV 0..1) — нижняя
/// половина экрана при `quad_projection`.
fn draw_textured_triangle(ctx: &Context<'_>) {
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.color3f(1.0, 1.0, 1.0);
    ctx.tex_coord2f(0.0, 0.0);
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.tex_coord2f(1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.tex_coord2f(0.0, 1.0);
    ctx.vertex3f(-1.0, 1.0, 0.0);
    ctx.end().expect("end");
}

fn render_textured_triangle() -> Frame {
    frame(CLEAR, |ctx| {
        quad_projection(ctx);
        bind_quadrant_texture(ctx);
        draw_textured_triangle(ctx);
    })
}

/// Полноэкранный текстурный quad (`GL_QUADS`, UV 0..1).
fn draw_textured_quad(ctx: &Context<'_>) {
    ctx.begin(Primitive::Quads).expect("begin");
    ctx.color3f(1.0, 1.0, 1.0);
    ctx.tex_coord2f(0.0, 0.0);
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.tex_coord2f(1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.tex_coord2f(1.0, 1.0);
    ctx.vertex3f(1.0, 1.0, 0.0);
    ctx.tex_coord2f(0.0, 1.0);
    ctx.vertex3f(-1.0, 1.0, 0.0);
    ctx.end().expect("end");
}

fn render_textured_quad() -> Frame {
    frame(CLEAR, |ctx| {
        quad_projection(ctx);
        bind_quadrant_texture(ctx);
        draw_textured_quad(ctx);
    })
}

fn cube_faces() -> [[[f32; 3]; 4]; 6] {
    [
        // +Z
        [
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ],
        // -Z
        [
            [1.0, -1.0, -1.0],
            [-1.0, -1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [1.0, 1.0, -1.0],
        ],
        // +X
        [
            [1.0, -1.0, 1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [1.0, 1.0, 1.0],
        ],
        // -X
        [
            [-1.0, -1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [-1.0, 1.0, 1.0],
            [-1.0, 1.0, -1.0],
        ],
        // +Y
        [
            [-1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
        ],
        // -Y
        [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, -1.0, 1.0],
            [-1.0, -1.0, 1.0],
        ],
    ]
}

fn draw_textured_cube(ctx: &Context<'_>) {
    ctx.begin(Primitive::Quads).expect("begin");
    for face in cube_faces() {
        for (vertex, uv) in face
            .iter()
            .zip([[0.0f32, 0.0f32], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
        {
            ctx.color3f(1.0, 1.0, 1.0);
            ctx.tex_coord2f(uv[0], uv[1]);
            ctx.vertex3f(vertex[0], vertex[1], vertex[2]);
        }
    }
    ctx.end().expect("end");
}

/// Текстурированный куб, кадр `n`: угол `n * 15°`, фиксированный порядок
/// (кадры по счёту, не по времени).
fn render_textured_cube(frame_index: u32) -> Frame {
    let angle = frame_index as f32 * 15.0;
    frame(CLEAR, |ctx| {
        ctx.matrix_mode(MatrixMode::Projection);
        ctx.load_identity();
        ctx.perspective(60.0, 1.0, 1.0, 20.0).expect("perspective");
        ctx.matrix_mode(MatrixMode::ModelView);
        ctx.load_identity();
        ctx.translate(0.0, 0.0, -4.0).expect("translate");
        ctx.rotate(angle, 1.0, 1.0, 0.5).expect("rotate");
        bind_quadrant_texture(ctx);
        draw_textured_cube(ctx);
    })
}

// --- пиксельные эталоны (закреплены после детерминированного прогона) ------

const TRIANGLE_DIGEST: u64 = 0xf65a_e5cc_db28_d083;
const TRIANGLE_CENTER: u32 = 0x0000_ff00;
const TRIANGLE_COVERAGE: usize = 2080;
const QUAD_DIGEST: u64 = 0xd423_7472_7add_c383;
const QUAD_CENTER: u32 = 0x0000_ff00;
const QUAD_COVERAGE: usize = 4096;
/// Квадранты текстуры на экране: загруженная строка 0 оказывается **внизу**
/// (t=0 — низ, как в GL: TinyGL не переворачивает данные).
const QUAD_TOP_LEFT: u32 = 0x0000_00ff; // source (x<4, y>=4) — синий
const QUAD_TOP_RIGHT: u32 = 0x00ff_ffff; // source (x>=4, y>=4) — белый
const QUAD_BOTTOM_LEFT: u32 = 0x00ff_0000; // source (x<4, y<4) — красный
const QUAD_BOTTOM_RIGHT: u32 = 0x0000_ff00; // source (x>=4, y<4) — зелёный
const CUBE_DIGESTS: [u64; 6] = [
    0x72e1_d214_59ce_58e1,
    0x07f0_6296_f1d4_e322,
    0x8951_d89d_a48f_33bb,
    0xcdfa_a125_7a63_ab64,
    0x8edf_ac07_e245_ebbd,
    0xe409_90fc_ec6b_8835,
];
const CUBE_CENTER: [u32; 6] = [
    0x0000_ff00,
    0x0000_ff00,
    0x0000_ff00,
    0x0000_ff00,
    0x00ff_0000,
    0x00ff_0000,
];
const CUBE_COVERAGE: [usize; 6] = [1444, 1289, 1226, 1321, 1352, 1288];

// --- текстура: кадр и дайджест -------------------------------------------

#[test]
fn textured_quad_matches_criterion() {
    let _guard = gl_test_lock();
    let rendered = render_textured_quad();
    if rendered.center() != QUAD_CENTER || rendered.coverage() != QUAD_COVERAGE {
        panic!(
            "textured quad GEOMETRY changed (center 0x{:08x} vs 0x{QUAD_CENTER:08x}, \
             coverage {} vs {QUAD_COVERAGE}) — not FP reassociation; BLOCK until classified",
            rendered.center(),
            rendered.coverage()
        );
    }
    assert_eq!(
        rendered.digest, QUAD_DIGEST,
        "textured quad digest diverged while geometry is identical — classify as colour/\
         interpolation change and record both digests (host/musl) before accepting"
    );
    assert_eq!(
        rendered.corner(),
        QUAD_TOP_LEFT,
        "full-screen textured quad covers the corner (no clear colour left)"
    );
    assert_eq!(rendered.center() >> 24, 0, "pixels must stay 0x00RRGGBB");
}

#[test]
fn textured_triangle_matches_criterion() {
    let _guard = gl_test_lock();
    let rendered = render_textured_triangle();
    if rendered.center() != TRIANGLE_CENTER || rendered.coverage() != TRIANGLE_COVERAGE {
        panic!(
            "textured triangle GEOMETRY changed (center 0x{:08x} vs 0x{TRIANGLE_CENTER:08x}, \
             coverage {} vs {TRIANGLE_COVERAGE}) — BLOCK until classified",
            rendered.center(),
            rendered.coverage()
        );
    }
    assert_eq!(
        rendered.digest, TRIANGLE_DIGEST,
        "textured triangle digest diverged with identical geometry — classify and record both \
         host/musl digests before accepting"
    );
}

#[test]
fn textured_triangle_is_deterministic() {
    let _guard = gl_test_lock();
    let first = render_textured_triangle();
    let second = render_textured_triangle();
    assert_eq!(first.digest, second.digest);
    assert_eq!(first.pixels, second.pixels);
}

#[test]
fn texture_quadrants_and_orientation() {
    let _guard = gl_test_lock();
    let rendered = render_textured_quad();
    assert_eq!(rendered.at(8, 8), QUAD_TOP_LEFT);
    assert_eq!(rendered.at(56, 8), QUAD_TOP_RIGHT);
    assert_eq!(rendered.at(8, 56), QUAD_BOTTOM_LEFT);
    assert_eq!(rendered.at(56, 56), QUAD_BOTTOM_RIGHT);
    assert_eq!(
        rendered.coverage(),
        W as usize * H as usize,
        "full-screen quad must touch every pixel"
    );
}

#[test]
fn textured_quad_is_deterministic() {
    let _guard = gl_test_lock();
    let first = render_textured_quad();
    let second = render_textured_quad();
    assert_eq!(first.digest, second.digest);
    assert_eq!(first.pixels, second.pixels);
}

#[test]
fn texture_replaces_vertex_colour_in_decal_mode() {
    let _guard = gl_test_lock();
    let white = frame(CLEAR, |ctx| {
        quad_projection(ctx);
        bind_quadrant_texture(ctx);
        draw_textured_quad(ctx);
    });
    let magenta = frame(CLEAR, |ctx| {
        quad_projection(ctx);
        bind_quadrant_texture(ctx);
        ctx.begin(Primitive::Quads).expect("begin");
        ctx.color3f(1.0, 0.0, 1.0);
        ctx.tex_coord2f(0.0, 0.0);
        ctx.vertex3f(-1.0, -1.0, 0.0);
        ctx.color3f(0.0, 1.0, 0.0);
        ctx.tex_coord2f(1.0, 0.0);
        ctx.vertex3f(1.0, -1.0, 0.0);
        ctx.color3f(0.0, 0.0, 1.0);
        ctx.tex_coord2f(1.0, 1.0);
        ctx.vertex3f(1.0, 1.0, 0.0);
        ctx.color3f(1.0, 1.0, 0.0);
        ctx.tex_coord2f(0.0, 1.0);
        ctx.vertex3f(-1.0, 1.0, 0.0);
        ctx.end().expect("end");
    });
    assert_eq!(
        white.pixels, magenta.pixels,
        "GL_DECAL on 32-bit TinyGL writes the texel and ignores vertex colour"
    );
}

#[test]
fn textured_cube_frames_match_criterion() {
    let _guard = gl_test_lock();
    for frame_index in 0..6u32 {
        let rendered = render_textured_cube(frame_index);
        let center = CUBE_CENTER[frame_index as usize];
        let coverage = CUBE_COVERAGE[frame_index as usize];
        if rendered.center() != center || rendered.coverage() != coverage {
            panic!(
                "textured cube frame {frame_index} GEOMETRY changed (center 0x{:08x} vs \
                 0x{center:08x}, coverage {} vs {coverage}) — BLOCK until classified",
                rendered.center(),
                rendered.coverage()
            );
        }
        assert_eq!(
            rendered.digest, CUBE_DIGESTS[frame_index as usize],
            "textured cube frame {frame_index} digest diverged with identical geometry — \
             classify (colour/interpolation) and record host+musl digests before accepting"
        );
    }
}

#[test]
fn textured_cube_frames_are_deterministic_and_distinct() {
    let _guard = gl_test_lock();
    let mut digests = Vec::new();
    for frame_index in 0..6u32 {
        let first = render_textured_cube(frame_index);
        let second = render_textured_cube(frame_index);
        assert_eq!(
            first.digest, second.digest,
            "frame {frame_index} must hash identically on two runs"
        );
        assert_eq!(first.pixels, second.pixels);
        digests.push(first.digest);
    }
    assert!(
        digests.windows(2).any(|pair| pair[0] != pair[1]),
        "rotation frames must not collapse to one digest: {digests:x?}"
    );
}

// --- текстура: API и плохие входы ----------------------------------------

#[test]
fn texture_api_requires_bind_and_image() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");

    assert_eq!(
        ctx.enable_texture_2d(),
        Err(FbError::TextureNotBound),
        "no bind -> no texture object"
    );
    let names = ctx.gen_textures(1).expect("one name");
    ctx.bind_texture(names[0]).expect("bind");
    assert_eq!(
        ctx.enable_texture_2d(),
        Err(FbError::TextureHasNoImage { texture: names[0] }),
        "bind without glTexImage2D must not enable the texture"
    );
    ctx.tex_image_2d(4, 4, &vec![7u8; 4 * 4 * 3])
        .expect("upload");
    ctx.enable_texture_2d().expect("now it has an image");
    ctx.disable_texture_2d();
}

#[test]
fn tex_image_rejects_bad_input() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");

    let names = ctx.gen_textures(1).expect("one name");
    ctx.bind_texture(names[0]).expect("bind");
    assert_eq!(
        ctx.tex_image_2d(0, 4, &[]),
        Err(FbError::BadTextureSize {
            width: 0,
            height: 4
        })
    );
    assert_eq!(
        ctx.tex_image_2d(4, 0, &[]),
        Err(FbError::BadTextureSize {
            width: 4,
            height: 0
        })
    );
    assert_eq!(
        ctx.tex_image_2d(4097, 4, &vec![0; 4097 * 4 * 3]),
        Err(FbError::BadTextureSize {
            width: 4097,
            height: 4
        })
    );
    assert_eq!(
        ctx.tex_image_2d(4, 4, &[0u8; 47]),
        Err(FbError::TextureDataLenMismatch {
            expected: 48,
            actual: 47
        })
    );
    assert_eq!(
        ctx.tex_image_2d(4, 4, &[0u8; 49]),
        Err(FbError::TextureDataLenMismatch {
            expected: 48,
            actual: 49
        })
    );
}

#[test]
fn tex_image_without_bind_is_rejected() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");
    assert_eq!(
        ctx.tex_image_2d(4, 4, &[0u8; 48]),
        Err(FbError::TextureNotBound)
    );
}

#[test]
fn tex_parameter_accepts_only_upstream_subset() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");
    ctx.tex_parameter(TexturePname::WrapS, TextureParam::Repeat)
        .expect("wrap S repeat");
    ctx.tex_parameter(TexturePname::WrapT, TextureParam::Repeat)
        .expect("wrap T repeat");
    ctx.tex_parameter(TexturePname::MinFilter, TextureParam::Nearest)
        .expect("min nearest");
    ctx.tex_parameter(TexturePname::MagFilter, TextureParam::Nearest)
        .expect("mag nearest");
    // Несовместимые пары отвергаются нашим слоем, а не exit(1) в C.
    assert!(matches!(
        ctx.tex_parameter(TexturePname::WrapS, TextureParam::Nearest),
        Err(FbError::UnsupportedTextureParameter { .. })
    ));
    assert!(matches!(
        ctx.tex_parameter(TexturePname::MinFilter, TextureParam::Repeat),
        Err(FbError::UnsupportedTextureParameter { .. })
    ));
    ctx.tex_env(TextureEnvMode::Decal);
}

#[test]
fn gen_and_delete_textures() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");

    assert_eq!(
        ctx.gen_textures(0).err(),
        Some(FbError::BadTextureCount { count: 0 })
    );
    assert_eq!(
        ctx.gen_textures(1025).err(),
        Some(FbError::BadTextureCount { count: 1025 })
    );
    let a = ctx.gen_textures(2).expect("two names");
    assert_eq!(a, vec![1, 2], "upstream names start at max+1");
    let b = ctx.gen_textures(1).expect("one more name");
    assert_eq!(
        b,
        vec![1],
        "upstream names are max(existing object)+1; unbound names are not objects"
    );

    ctx.bind_texture(a[0]).expect("bind");
    assert_eq!(
        ctx.bind_texture(u32::MAX).err(),
        Some(FbError::BadTextureName { name: u32::MAX })
    );
    ctx.tex_image_2d(2, 2, &vec![1u8; 12]).expect("upload");

    // Удаление несвязанной текстуры.
    ctx.delete_textures(&[a[1]]).expect("delete unbound");
    // Имя 0 игнорируется (upstream UAF), текущая текстура остаётся.
    ctx.delete_textures(&[0]).expect("delete 0 is ignored");
    ctx.tex_image_2d(2, 2, &vec![2u8; 12])
        .expect("still bound to 1");
    ctx.disable_texture_2d();
    // Удаление связанной текстуры при выключенной текстуре: C биндит 0.
    ctx.delete_textures(&[a[0]])
        .expect("delete bound while disabled");
    assert_eq!(
        ctx.enable_texture_2d(),
        Err(FbError::TextureHasNoImage { texture: 0 }),
        "after deleting the bound texture C is bound to the incomplete name 0"
    );
}

#[test]
fn deleting_bound_texture_while_enabled_is_rejected() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");
    let names = ctx.gen_textures(1).expect("name");
    ctx.bind_texture(names[0]).expect("bind");
    ctx.tex_image_2d(2, 2, &vec![3u8; 12]).expect("upload");
    ctx.enable_texture_2d().expect("enable");
    assert_eq!(
        ctx.delete_textures(&[names[0]]),
        Err(FbError::TextureBoundAndEnabled { texture: names[0] })
    );
    // Несвязанную текстуру удалить можно.
    ctx.delete_textures(&[999])
        .expect("unbound name is a no-op");
}

#[test]
fn texture_matrix_mode_has_its_own_stack() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");

    ctx.matrix_mode(MatrixMode::Texture);
    assert_eq!(ctx.current_matrix_mode(), MatrixMode::Texture);
    ctx.load_identity();
    ctx.translate(0.25, 0.5, 0.0).expect("finite translate");
    let texture_matrix = ctx.get_matrix(MatrixMode::Texture);
    assert_eq!(
        (texture_matrix[12], texture_matrix[13], texture_matrix[14]),
        (0.25, 0.5, 0.0)
    );
    assert_eq!(ctx.max_texture_size(), 256);

    // Предел texture-стека — 8, как в zgl.h.
    for _ in 0..7 {
        ctx.push_matrix().expect("push within texture stack");
    }
    assert_eq!(
        ctx.push_matrix(),
        Err(FbError::MatrixStackOverflow { mode: "texture" })
    );
    for _ in 0..7 {
        ctx.pop_matrix().expect("pop");
    }
    assert_eq!(
        ctx.pop_matrix(),
        Err(FbError::MatrixStackUnderflow { mode: "texture" })
    );
    ctx.matrix_mode(MatrixMode::ModelView);
}

// --- текстуры внутри display-list запрещены ------------------------------

#[test]
fn client_pointer_operations_are_rejected_during_compilation() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");
    let first = ctx.gen_lists(1).expect("list name");
    ctx.new_list(first, tinygl::ListMode::Compile)
        .expect("new list");

    assert_eq!(
        ctx.tex_image_2d(2, 2, &[0u8; 12]),
        Err(FbError::ListClientPointer)
    );
    assert_eq!(ctx.gen_textures(1).err(), Some(FbError::ListClientPointer));
    assert_eq!(ctx.delete_textures(&[7]), Err(FbError::ListClientPointer));
    assert_eq!(ctx.gen_lists(1).err(), Some(FbError::ListClientPointer));
    assert_eq!(ctx.delete_lists(0, 1), Err(FbError::ListClientPointer));
    ctx.end_list().expect("end list");
}
