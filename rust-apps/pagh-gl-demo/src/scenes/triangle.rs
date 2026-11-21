//! `triangle` — минимальный proof: perspective + clip + Gouraud-растеризация.
//!
//! Один треугольник с цветом в каждой вершине и фиксированным углом. Кадр не
//! меняется (спека §6.1: «фиксированный угол»), поэтому `--frames 3` даёт три
//! одинаковых дайджеста — это часть контракта, а не баг.

use tinygl::{Capability, Context, FbError, Primitive};

use super::{enable_depth, perspective};

pub fn draw(ctx: &Context<'_>) -> Result<(), FbError> {
    perspective(ctx, 55.0)?;
    ctx.translate(0.0, -0.1, -4.0)?;
    // Чуть повёрнутый треугольник: видно, что матрицы реально применяются,
    // но угол — константа, кадр детерминирован.
    ctx.rotate(-12.0, 0.0, 0.0, 1.0)?;
    ctx.rotate(14.0, 1.0, 0.0, 0.0)?;
    enable_depth(ctx);
    ctx.disable(Capability::CullFace);
    ctx.set_smooth_shading(true);

    ctx.begin(Primitive::Triangles)?;
    ctx.color3f(1.0, 0.15, 0.1);
    ctx.vertex3f(-1.55, -1.1, 0.0);
    ctx.color3f(0.15, 1.0, 0.25);
    ctx.vertex3f(1.55, -1.1, 0.0);
    ctx.color3f(0.15, 0.35, 1.0);
    ctx.vertex3f(0.0, 1.55, 0.0);
    ctx.end()
}
