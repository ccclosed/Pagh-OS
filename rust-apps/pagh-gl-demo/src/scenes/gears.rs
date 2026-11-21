//! `gears` — нагрузочная сцена: три зубчатых колеса, ~1.1k треугольников,
//! освещение `off`/`on`.
//!
//! Профиль зуба **написан заново** под наш immediate-mode API (не копия
//! upstream `examples/gears.c`, который к тому же GPL-несовместим с MIT-деревом):
//! на каждый зуб четыре точки — впадина, два угла вершины и снова впадина —
//! плюс внутреннее отверстие. Крышки (кольца), наружные и внутренние стенки
//! дают по два треугольника на квад.
//!
//! ## Детерминизм
//!
//! Базовый профиль считается [`super::sin_cos_turns`] (свой ряд Тейлора без
//! libm), а поворот кадра уходит в C `glRotatef`. Поэтому геометрия одна и та
//! же на glibc-хосте и musl-госте — иначе 1-ulp разница в libm сдвинула бы
//! вершину и сломала pixel-эталон.
//!
//! ## Освещение
//!
//! `lighting = true` включает `GL_LIGHTING`/`GL_LIGHT0`, материал каждой
//! шестерёнки и нормали; `off` — плоский `glColor3f` без света. Это ровно два
//! пути, которые проверяет спека §6.1.

use tinygl::{
    Capability, Context, FbError, Light, LightParam, MaterialFace, MaterialParam, Primitive,
};

use super::{enable_depth, perspective, sin_cos_turns, GEAR_STEP_DEGREES};

/// Модуль зуба: чем больше, тем крупнее зубья относительно радиуса.
const MODULE: f32 = 0.28;
/// Полутолщина колеса по Z.
const HALF_THICKNESS: f32 = 0.30;

struct GearSpec {
    teeth: u32,
    center: [f32; 2],
    /// Передаточное отношение к первой шестерёнке (знак — направление).
    ratio: f32,
    color: [f32; 3],
}

const GEARS: [GearSpec; 3] = [
    GearSpec {
        teeth: 12,
        center: [-2.2, 0.9],
        ratio: 1.0,
        color: [0.85, 0.45, 0.15],
    },
    GearSpec {
        teeth: 14,
        center: [2.2, 0.9],
        ratio: -12.0 / 14.0,
        color: [0.45, 0.55, 0.75],
    },
    GearSpec {
        teeth: 10,
        center: [2.2, -3.2],
        ratio: 12.0 / 10.0,
        color: [0.85, 0.70, 0.20],
    },
];

/// Радиусы колеса: (впадина, вершина зуба, отверстие).
fn radii(teeth: u32) -> (f64, f64, f64) {
    let pitch = f64::from(MODULE) * f64::from(teeth) / 2.0;
    let root = pitch - 1.25 * f64::from(MODULE);
    let tip = pitch + f64::from(MODULE);
    let hole = (root * 0.45).max(0.22);
    (root, tip, hole)
}

/// Внешний и внутренний контуры колеса. Оба содержат по `4 * teeth` точек;
/// `i`-я точка внутреннего контура лежит на том же луче, что и внешняя.
fn profile(teeth: u32) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let (root, tip, hole) = radii(teeth);
    let step = 1.0 / f64::from(teeth);
    // Четыре точки на зуб: впадина → угол вершины → угол вершины → впадина.
    const PROFILE: [(f64, u8); 4] = [(0.00, 0), (0.18, 1), (0.42, 1), (0.60, 0)];

    let count = 4 * teeth as usize;
    let mut outer = Vec::with_capacity(count);
    let mut inner = Vec::with_capacity(count);
    for tooth in 0..teeth {
        for (fraction, which) in PROFILE {
            let angle = f64::from(tooth) * step + fraction * step;
            let (sin, cos) = sin_cos_turns(angle);
            let radius = if which == 1 { tip } else { root };
            outer.push([(cos * radius) as f32, (sin * radius) as f32]);
            inner.push([(cos * hole) as f32, (sin * hole) as f32]);
        }
    }
    (outer, inner)
}

pub fn draw(ctx: &Context<'_>, frame: u32, lighting: bool) -> Result<(), FbError> {
    perspective(ctx, 50.0)?;
    ctx.translate(0.0, -0.2, -16.0)?;
    enable_depth(ctx);
    // Culling для шестерёнок не нужен: крышки/стенки смотрят в обе стороны, а
    // Z-буфер решает порядок. Включать его здесь значило бы зависеть от
    // обхода каждого квада — лишний риск без выигрыша на 1.1k треугольников.
    ctx.disable(Capability::CullFace);
    ctx.set_smooth_shading(false);

    if lighting {
        // Позиция источника — после камеры и до поворотов колёс: upstream
        // умножает POSITION на текущую modelview в момент вызова.
        ctx.light(Light::Light0, LightParam::Position, [0.35, 0.55, 1.0, 0.0])?;
        ctx.light(Light::Light0, LightParam::Diffuse, [1.0, 1.0, 1.0, 1.0])?;
        ctx.light(Light::Light0, LightParam::Ambient, [0.16, 0.16, 0.18, 1.0])?;
        ctx.light(Light::Light0, LightParam::Specular, [0.9, 0.9, 0.9, 1.0])?;
        ctx.enable(Capability::Lighting);
        ctx.enable(Capability::Light0);
    } else {
        ctx.disable(Capability::Lighting);
        ctx.disable(Capability::Light0);
    }

    for gear in &GEARS {
        let angle = frame as f32 * GEAR_STEP_DEGREES * gear.ratio;
        ctx.push_matrix()?;
        ctx.translate(gear.center[0], gear.center[1], 0.0)?;
        ctx.rotate(angle, 0.0, 0.0, 1.0)?;
        if lighting {
            ctx.material(
                MaterialFace::FrontAndBack,
                MaterialParam::AmbientAndDiffuse,
                [gear.color[0], gear.color[1], gear.color[2], 1.0],
            )?;
            ctx.material(
                MaterialFace::FrontAndBack,
                MaterialParam::Specular,
                [0.25, 0.25, 0.25, 1.0],
            )?;
            ctx.material(
                MaterialFace::FrontAndBack,
                MaterialParam::Shininess,
                [24.0, 0.0, 0.0, 0.0],
            )?;
        }
        draw_gear(ctx, gear.teeth, gear.color, lighting)?;
        ctx.pop_matrix()?;
    }
    Ok(())
}

fn draw_gear(
    ctx: &Context<'_>,
    teeth: u32,
    color: [f32; 3],
    lighting: bool,
) -> Result<(), FbError> {
    let (outer, inner) = profile(teeth);
    let count = outer.len();
    let z0 = -HALF_THICKNESS;
    let z1 = HALF_THICKNESS;

    ctx.begin(Primitive::Quads)?;
    if !lighting {
        ctx.color3f(color[0], color[1], color[2]);
    }
    for i in 0..count {
        let j = (i + 1) % count;
        let oi = outer[i];
        let oj = outer[j];
        let ii = inner[i];
        let ij = inner[j];

        // Задняя крышка (z0), нормаль -Z.
        quad(
            ctx,
            [0.0, 0.0, -1.0],
            [ii[0], ii[1], z0],
            [ij[0], ij[1], z0],
            [oj[0], oj[1], z0],
            [oi[0], oi[1], z0],
        );
        // Передняя крышка (z1), нормаль +Z.
        quad(
            ctx,
            [0.0, 0.0, 1.0],
            [ii[0], ii[1], z1],
            [oi[0], oi[1], z1],
            [oj[0], oj[1], z1],
            [ij[0], ij[1], z1],
        );

        // Наружная стенка: нормаль — среднее радиальное направление.
        let outer_normal = radial_normal(oi, oj, 1.0);
        quad(
            ctx,
            outer_normal,
            [oi[0], oi[1], z0],
            [oj[0], oj[1], z0],
            [oj[0], oj[1], z1],
            [oi[0], oi[1], z1],
        );

        // Внутренняя стенка отверстия: нормаль смотрит к оси.
        let inner_normal = radial_normal(ii, ij, -1.0);
        quad(
            ctx,
            inner_normal,
            [ii[0], ii[1], z0],
            [ii[0], ii[1], z1],
            [ij[0], ij[1], z1],
            [ij[0], ij[1], z0],
        );
    }
    ctx.end()
}

/// Нормаль стенки между точками `a` и `b` профиля: единичное среднее
/// направление, умноженное на `sign` (наружу +1, к оси -1).
fn radial_normal(a: [f32; 2], b: [f32; 2], sign: f32) -> [f32; 3] {
    let x = (a[0] + b[0]) * 0.5;
    let y = (a[1] + b[1]) * 0.5;
    let len = (x * x + y * y).sqrt();
    if len == 0.0 {
        return [0.0, 0.0, sign];
    }
    [sign * x / len, sign * y / len, 0.0]
}

fn quad(ctx: &Context<'_>, normal: [f32; 3], a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3]) {
    ctx.normal3f(normal[0], normal[1], normal[2]);
    ctx.vertex3f(a[0], a[1], a[2]);
    ctx.normal3f(normal[0], normal[1], normal[2]);
    ctx.vertex3f(b[0], b[1], b[2]);
    ctx.normal3f(normal[0], normal[1], normal[2]);
    ctx.vertex3f(c[0], c[1], c[2]);
    ctx.normal3f(normal[0], normal[1], normal[2]);
    ctx.vertex3f(d[0], d[1], d[2]);
}
