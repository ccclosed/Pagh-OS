//! `cube` — proof Z-буфера и back-face culling.
//!
//! Шесть граней с разными цветами; кадр `n` повёрнут на `n * 15°` вокруг
//! фиксированной оси. 15° — не произвол: на этом шаге PR3 зафиксировал
//! совпадение host/musl-дайджестов, поэтому живое окно и host-эталон можно
//! сравнивать без классификации FP-расхождений.

use tinygl::{Capability, Context, CullFace, FbError, FrontFace, Primitive};

use super::{enable_depth, perspective, CUBE_STEP_DEGREES};

/// Грани куба `[-1, 1]³`; порядок вершин — CCW при взгляде **снаружи**
/// (проверено векторным произведением в host-тесте geometry).
const FACES: [([[f32; 3]; 4], [f32; 3]); 6] = [
    // +Z
    (
        [
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ],
        [0.95, 0.20, 0.15],
    ),
    // -Z
    (
        [
            [1.0, -1.0, -1.0],
            [-1.0, -1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [1.0, 1.0, -1.0],
        ],
        [0.15, 0.85, 0.25],
    ),
    // +X
    (
        [
            [1.0, -1.0, 1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [1.0, 1.0, 1.0],
        ],
        [0.20, 0.35, 0.95],
    ),
    // -X
    (
        [
            [-1.0, -1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [-1.0, 1.0, 1.0],
            [-1.0, 1.0, -1.0],
        ],
        [0.95, 0.80, 0.15],
    ),
    // +Y
    (
        [
            [-1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
        ],
        [0.15, 0.80, 0.85],
    ),
    // -Y
    (
        [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, -1.0, 1.0],
            [-1.0, -1.0, 1.0],
        ],
        [0.85, 0.20, 0.85],
    ),
];

pub fn draw(ctx: &Context<'_>, frame: u32) -> Result<(), FbError> {
    perspective(ctx, 55.0)?;
    ctx.translate(0.0, 0.0, -5.2)?;
    // Базовая ориентация (фиксированная) показывает три грани уже на кадре 0;
    // анимируется только азимут: кадр n = 25° + n * 15° вокруг Y.
    ctx.rotate(25.0 + frame as f32 * CUBE_STEP_DEGREES, 0.0, 1.0, 0.0)?;
    ctx.rotate(18.0, 1.0, 0.0, 0.0)?;
    enable_depth(ctx);
    ctx.enable(Capability::CullFace);
    ctx.set_cull_face(CullFace::Back);
    ctx.set_front_face(FrontFace::Ccw);
    ctx.set_smooth_shading(false);

    ctx.begin(Primitive::Quads)?;
    for (face, color) in FACES {
        ctx.color3f(color[0], color[1], color[2]);
        for vertex in face {
            ctx.vertex3f(vertex[0], vertex[1], vertex[2]);
        }
    }
    ctx.end()
}
