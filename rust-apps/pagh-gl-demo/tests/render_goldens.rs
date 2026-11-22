//! PR5 pixel checks: FNV-1a-64 дайджесты трёх сцен + точечные проверки
//! геометрии (центр треугольника, угол куба, «дырка» шестерёнки).
//!
//! Эталоны сняты с host-прогона (glibc, `-O2` C TinyGL) и **обязаны** совпасть
//! на musl-госте: геометрия считается либо C (матрицы/`glRotatef`), либо нашим
//! детерминированным рядом без libm (`gears`), поэтому FP-расхождений быть не
//! должно. Тест сначала проверяет геометрию/цвет, и только потом дайджест:
//! расхождение геометрии — FAIL, а не «FP-приближение».
//!
//! Тесты, открывающие `Context`, сериализуются мьютексом: `gl_ctx` в TinyGL
//! глобальный.

use std::sync::{Mutex, MutexGuard};

use pagh_gl_demo::{pixel_digest, render_headless, SceneKind};

static GL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn gl_test_lock() -> MutexGuard<'static, ()> {
    GL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

const W: u32 = 640;
const H: u32 = 480;

const TRIANGLE_DIGEST: u64 = 0x13e6_9817_40ce_9b4d;
const TRIANGLE_CENTER: u32 = 0x0067_7a83;

const CUBE_DIGESTS: [u64; 3] = [
    0xedbd_26e7_dcbf_0507,
    0x6bf2_cc8b_656d_31a5,
    0x921e_af89_2e9d_44a5,
];
const CUBE_CENTER: u32 = 0x00ef_3429;

const GEARS_LIT_DIGESTS: [u64; 2] = [0x3d6f_cc42_8ccb_b634, 0x2a04_fc59_b472_2ea0];
const GEARS_UNLIT_DIGESTS: [u64; 2] = [0x900c_a56f_270b_8033, 0xe8d5_da96_52a9_eb7b];

/// Кадр `n` сцены `scene` (headless, 640×480).
fn frame(scene: SceneKind, index: u32, lighting: bool) -> (u64, Vec<u32>) {
    let rendered =
        render_headless(scene, W, H, index + 1, lighting, false).expect("headless render");
    (rendered.digests[index as usize], rendered.last)
}

fn pixel(pixels: &[u32], x: u32, y: u32) -> u32 {
    pixels[(y * W + x) as usize]
}

#[test]
fn triangle_digest_and_gouraud_geometry_match_the_reference() {
    let _guard = gl_test_lock();
    let (digest, pixels) = frame(SceneKind::Triangle, 0, true);
    assert_eq!(
        digest, TRIANGLE_DIGEST,
        "triangle frame 0 digest diverged from the pinned reference"
    );
    assert_eq!(
        pixel(&pixels, 320, 240),
        TRIANGLE_CENTER,
        "triangle centre must be the interpolated Gouraud colour"
    );
    assert_eq!(pixel(&pixels, 0, 0), 0, "corner must stay clear");
    assert_eq!(
        pixel(&pixels, 320, 240) >> 24,
        0,
        "TinyGL pixel must be 0x00RRGGBB (XRGB8888), not ARGB"
    );
}

#[test]
fn triangle_is_static_and_deterministic() {
    let _guard = gl_test_lock();
    let first = frame(SceneKind::Triangle, 7, true);
    let second = frame(SceneKind::Triangle, 7, true);
    assert_eq!(first.0, second.0);
    assert_eq!(first.1, second.1, "two renders must be pixel-identical");
    assert_eq!(
        first.0, TRIANGLE_DIGEST,
        "triangle has a fixed angle: frame number must not change the image"
    );
}

#[test]
fn cube_frames_are_pinned_and_rotate_by_count() {
    let _guard = gl_test_lock();
    let mut previous = None;
    for (index, expected) in CUBE_DIGESTS.iter().enumerate() {
        let (digest, pixels) = frame(SceneKind::Cube, index as u32, true);
        assert_eq!(digest, *expected, "cube frame {index} digest diverged");
        assert_eq!(pixel(&pixels, 0, 0), 0, "corner must stay clear");
        if let Some(previous) = previous {
            assert_ne!(previous, digest, "animated cube frames must differ");
        }
        previous = Some(digest);
    }
    // Кадр 0 — фиксированная ориентация, видно три грани; центр — красная
    // грань +Z. Это локализует баг culling/depth лучше одного хеша.
    let (_, pixels) = frame(SceneKind::Cube, 0, true);
    assert_eq!(pixel(&pixels, 320, 240), CUBE_CENTER);
}

#[test]
fn gears_lighting_on_and_off_have_distinct_pinned_digests() {
    let _guard = gl_test_lock();
    for (index, (lit, unlit)) in GEARS_LIT_DIGESTS
        .iter()
        .zip(GEARS_UNLIT_DIGESTS.iter())
        .enumerate()
    {
        let (lit_digest, lit_pixels) = frame(SceneKind::Gears, index as u32, true);
        let (unlit_digest, unlit_pixels) = frame(SceneKind::Gears, index as u32, false);
        assert_eq!(
            lit_digest, *lit,
            "gears frame {index} (lighting on) diverged"
        );
        assert_eq!(
            unlit_digest, *unlit,
            "gears frame {index} (lighting off) diverged"
        );
        assert_ne!(
            lit_digest, unlit_digest,
            "lighting on/off must produce different images"
        );
        assert_ne!(lit_pixels, unlit_pixels);
    }
}

#[test]
fn gears_have_a_hole_and_teeth_at_known_pixels() {
    let _guard = gl_test_lock();
    let (_, pixels) = frame(SceneKind::Gears, 0, true);
    // Центр второй шестерёнки (мир 2.2,0.9 при камере z=-16, fov 50) —
    // (391,205); радиус отверстия ~23 px, значит центр обязан быть фоном.
    assert_eq!(
        pixel(&pixels, 391, 205),
        0,
        "gear centre must show the through-hole, not a filled cap"
    );
    // Зуб на луче ~7.7° от центра B: точка (460,200) лежит на передней
    // крышке зуба и не может быть фоном.
    assert_ne!(
        pixel(&pixels, 460, 200),
        0,
        "a point on a tooth must be filled"
    );
    // Пространство между шестерёнками — фон.
    assert_eq!(pixel(&pixels, 320, 240), 0);
}

#[test]
fn headless_digest_is_independent_of_frame_count() {
    let _guard = gl_test_lock();
    let one = render_headless(SceneKind::Gears, W, H, 1, true, false).expect("one frame");
    let five = render_headless(SceneKind::Gears, W, H, 5, true, false).expect("five frames");
    assert_eq!(
        one.digests[0], five.digests[0],
        "frame 0 must not depend on how many frames are rendered"
    );
    assert_eq!(five.digests.len(), 5);
    assert_eq!(pixel_digest(&five.last), five.digests[4]);
}

#[test]
fn keep_all_returns_every_frame() {
    let _guard = gl_test_lock();
    let rendered = render_headless(SceneKind::Cube, 160, 120, 3, true, true).expect("frames");
    assert_eq!(rendered.all.len(), 3);
    assert_eq!(rendered.all[0].len(), 160 * 120);
    assert_ne!(rendered.all[0], rendered.all[1], "cube animates");
    assert_eq!(rendered.wall.len(), 3);
}

#[test]
fn rejects_surfaces_tinygl_cannot_express() {
    let _guard = gl_test_lock();
    assert!(render_headless(SceneKind::Triangle, 641, 480, 1, true, false).is_err());
    assert!(render_headless(SceneKind::Triangle, 640, 0, 1, true, false).is_err());
}
