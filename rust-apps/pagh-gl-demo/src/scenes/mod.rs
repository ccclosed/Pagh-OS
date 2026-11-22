//! Три детерминированные сцены демо: `triangle`, `cube`, `gears`.
//!
//! Каждая сцена — чистая функция от `(кадр, lighting)` и размера поверхности:
//! кадр `n` соответствует углу `n * step`, никакого wall-clock в логике кадра
//! нет (спека `docs/tinygl-port-spec.md` §6.2). Поэтому FNV-1a-дайджест кадра
//! воспроизводим и на хосте, и в QEMU-госте, и между musl/glibc-сборками.
//!
//! Сцены написаны под **наш** API (`tinygl::Context`, immediate mode), а не
//! скопированы из upstream `examples/`: `gears` — собственная генерация
//! зубчатого профиля (см. [`gears`]), `cube` — шесть граней с depth/culling,
//! `triangle` — минимальный Gouraud-proof.

pub mod cube;
pub mod gears;
pub mod triangle;

use tinygl::{Context, FbError, MatrixMode};

/// Какая сцена рисуется.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneKind {
    Triangle,
    Cube,
    Gears,
}

impl SceneKind {
    pub const ALL: [SceneKind; 3] = [SceneKind::Triangle, SceneKind::Cube, SceneKind::Gears];

    /// Имя для CLI/маркера `TINYGL <name>`.
    pub fn name(self) -> &'static str {
        match self {
            SceneKind::Triangle => "triangle",
            SceneKind::Cube => "cube",
            SceneKind::Gears => "gears",
        }
    }

    pub fn parse(value: &str) -> Option<SceneKind> {
        SceneKind::ALL
            .into_iter()
            .find(|scene| scene.name() == value)
    }

    /// Нарисовать кадр `frame` в уже открытый контекст.
    ///
    /// `lighting` учитывается только `gears` (у `triangle`/`cube` включение
    /// света изменило бы их эталонные дайджесты, а спека требует Gouraud и
    /// плоские грани соответственно).
    pub fn draw(self, ctx: &Context<'_>, frame: u32, lighting: bool) -> Result<(), FbError> {
        match self {
            SceneKind::Triangle => triangle::draw(ctx),
            SceneKind::Cube => cube::draw(ctx, frame),
            SceneKind::Gears => gears::draw(ctx, frame, lighting),
        }
    }
}

/// Шаг анимации куба: `15°` на кадр. Кратно проверен в PR3 (host↔musl
/// байт-в-байт), поэтому годится эталоном и для живого окна.
pub const CUBE_STEP_DEGREES: f32 = 15.0;

/// Шаг анимации шестерёнок: `1.5°` на кадр для первой; остальные вращаются в
/// передаточном отношении зубьев. Угол передаётся в C `glRotatef`, тригонометрия
/// кадра остаётся upstream-функцией.
pub const GEAR_STEP_DEGREES: f32 = 1.5;

/// Настроить перспективу и вернуть aspect (ширина/высота клиентской области).
pub(crate) fn perspective(ctx: &Context<'_>, fov_y: f64) -> Result<f32, FbError> {
    let width = ctx.surface().width() as f32;
    let height = ctx.surface().height() as f32;
    let aspect = width / height;
    ctx.matrix_mode(MatrixMode::Projection);
    ctx.load_identity();
    ctx.perspective(fov_y, f64::from(aspect), 0.5, 100.0)?;
    ctx.matrix_mode(MatrixMode::ModelView);
    ctx.load_identity();
    Ok(aspect)
}

/// Детерминированные `sin/cos` в оборотах (1.0 = 2π), только IEEE-754
/// сложение/умножение — **без libm**.
///
/// Зачем: базовый профиль шестерёнки вычисляется в Rust и обязан совпасть
/// бит-в-бит на glibc-хосте и musl-госте. `f32::sin_cos`/`f64::sin_cos` идут в
/// libm, и 1-ulp расхождение между реализациями сдвинуло бы вершину, а с ней
/// и пиксельный дайджест. Ряд Тейлора на `[-π/4, π/4]` после приведения к
/// квадранту даёт ~1e-9 точности — достаточно для геометрии и полностью
/// воспроизводимо на обоих таргетах.
pub(crate) fn sin_cos_turns(turns: f64) -> (f64, f64) {
    let turns = turns.rem_euclid(1.0);
    let quadrant = (turns * 4.0).floor();
    let fraction = turns * 4.0 - quadrant;
    let x = (fraction - 0.5) * (std::f64::consts::PI / 2.0);
    let x2 = x * x;

    // sin(x) = x * (1 - x²/3! + x⁴/5! - x⁶/7! + x⁸/9!)
    let sin = x
        * (1.0 - x2 / 6.0 + x2 * x2 / 120.0 - x2 * x2 * x2 / 5040.0
            + x2 * x2 * x2 * x2 / 362_880.0);
    // cos(x) = 1 - x²/2! + x⁴/4! - x⁶/6! + x⁸/8!
    let cos = 1.0 - x2 / 2.0 + x2 * x2 / 24.0 - x2 * x2 * x2 / 720.0 + x2 * x2 * x2 * x2 / 40_320.0;

    match quadrant as i32 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// Включить тестовый Z-буфер для залитой геометрии.
pub(crate) fn enable_depth(ctx: &Context<'_>) {
    ctx.enable(tinygl::Capability::DepthTest);
}
