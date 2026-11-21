//! Рендер-путь: headless (`Vec<u32>`) и живой (`mmap` ShmSurface).
//!
//! Оба пути используют один [`tinygl::Context`] на размер поверхности и
//! вызывают [`SceneKind::draw`] с номером кадра. Headless-путь нужен тестам и
//! `--out`/`--hash`; живой — PDP/1-клиенту.

use std::time::Instant;

use tinygl::{pixel_digest, Context, FbError, Surface};

use crate::scenes::SceneKind;
use crate::surface::SharedFrame;
use crate::timing::{now_cycles, Series};

/// Один живой GL-контекст поверх mmap-поверхности.
pub struct GlRenderer<'a> {
    ctx: Context<'a>,
    width: u32,
    height: u32,
}

impl<'a> GlRenderer<'a> {
    /// Открыть `ZB_open`/`glInit` поверх пикселей `frame`.
    ///
    /// `Surface::new` отвергает `width % 4 != 0` и неплотный буфер **до** C;
    /// `SharedFrame` создаётся только с `width % 4 == 0`.
    pub fn new(frame: &'a mut SharedFrame) -> Result<GlRenderer<'a>, FbError> {
        let width = frame.width();
        let height = frame.height();
        let surface = Surface::new(frame.pixels_mut(), width, height)?;
        let ctx = Context::new(surface)?;
        Ok(GlRenderer { ctx, width, height })
    }

    pub fn render(&self, scene: SceneKind, frame: u32, lighting: bool) -> Result<(), FbError> {
        self.ctx.set_viewport(0, 0, self.width, self.height)?;
        self.ctx.clear(0x0000_0000, 0);
        scene.draw(&self.ctx, frame, lighting)
    }

    pub fn pixels(&self) -> &[u32] {
        self.ctx.pixels()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/// Результат headless-прогона.
pub struct HeadlessFrames {
    pub width: u32,
    pub height: u32,
    pub digests: Vec<u64>,
    /// Пиксели последнего кадра.
    pub last: Vec<u32>,
    /// Пиксели всех кадров (только при `keep_all`).
    pub all: Vec<Vec<u32>>,
    /// Wall-clock рендера кадра (без `ZB_open`, контекст переиспользован).
    pub wall: Series,
    /// `rdtsc` рендера кадра.
    pub cycles: Series,
}

/// Отрендерить `frames` кадров сцены в `Vec<u32>` (headless, без композитора).
///
/// Контекст открывается **один раз**: замер тогда не включает `ZB_open`/
/// `glInit` каждого кадра (спека §6.4 отдельно оговаривает, что мерить надо
/// кадр, а не открытие окна).
pub fn render_headless(
    scene: SceneKind,
    width: u32,
    height: u32,
    frames: u32,
    lighting: bool,
    keep_all: bool,
) -> Result<HeadlessFrames, FbError> {
    let mut fb = vec![0u32; (width as usize) * (height as usize)];
    let mut digests = Vec::with_capacity(frames as usize);
    let mut all = if keep_all {
        Vec::with_capacity(frames as usize)
    } else {
        Vec::new()
    };
    let mut wall = Series::default();
    let mut cycles = Series::default();
    {
        let surface = Surface::new(&mut fb, width, height)?;
        let ctx = Context::new(surface)?;
        for frame in 0..frames {
            let started = Instant::now();
            let start_cycles = now_cycles();
            ctx.set_viewport(0, 0, width, height)?;
            ctx.clear(0x0000_0000, 0);
            scene.draw(&ctx, frame, lighting)?;
            cycles.push(now_cycles().saturating_sub(start_cycles) as f64);
            wall.push(started.elapsed().as_secs_f64() * 1000.0);
            digests.push(pixel_digest(ctx.pixels()));
            if keep_all {
                all.push(ctx.pixels().to_vec());
            }
        }
    }
    Ok(HeadlessFrames {
        width,
        height,
        digests,
        last: fb,
        all,
        wall,
        cycles,
    })
}
