//! Замерочный harness P0: сценарии нагрузки → frame time и breakdown.
//!
//! Harness не изобретает отдельный «бенчмарочный» рендер: он гоняет **тот же**
//! путь, что и сервер в [`crate::server::Server::render_if_needed`] —
//! `Compositor::render` (damage → paint_scene) и `Backend::present`
//! (blit damage в scanout). Сценарии двигают состояние композитора публичным
//! API (`pointer_motion`, `pointer_button`, `commit_surface`), то есть тем же,
//! каким его двигают evdev и PDP/1-клиенты.
//!
//! Четыре профиля нагрузки из плана:
//!
//! | сценарий | что происходит каждый кадр | чего ждём |
//! |---|---|---|
//! | `idle-full` | полная перерисовка статичного рабочего стола | цена полного кадра |
//! | `drag` | окно тянется за заголовок на 1-2 px | два прямоугольника окна |
//! | `cursor` | курсор идёт по кругу над рабочим столом | мелкий damage курсора |
//! | `files-scroll` | новое содержимое окна Files (скролл) | рект 560×340 |
//!
//! Первые [`WARMUP_FRAMES`] кадров каждого сценария отбрасываются: они строят
//! кэши (обои, панель, glyph-atlas) и стабилизируют hover, иначе среднее
//! смешивает холодный и горячий режим.
//!
//! Backend — реальный `FbdevBackend` в гостевом QEMU и [`MemoryBackend`] на
//! хосте: последний блитит в обычный `Vec<u8>` того же `FbLayout`, поэтому
//! `copy` меряет тот же `blit_damage`/`copy_row`, но без WC-памяти scanout.

use crate::backend::Backend;
use crate::compositor::{Compositor, MouseButton};
use crate::demo::{self, DemoState};
use crate::fbdev::{BitField, FbLayout};
use crate::geometry::{Point, Rect};
use crate::surface::PixelFormat;
use crate::timing::{self, Samples};
use std::io;

/// Сколько кадров прогрева отбрасывается в начале каждого сценария.
pub const WARMUP_FRAMES: usize = 4;

/// Размер экрана по умолчанию, если backend не сообщает свой.
pub const DEFAULT_SCREEN: (u32, u32) = (1280, 800);

/// Сценарий нагрузки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    IdleFull,
    Drag,
    Cursor,
    FilesScroll,
}

impl Scenario {
    pub const ALL: [Scenario; 4] = [
        Scenario::IdleFull,
        Scenario::Drag,
        Scenario::Cursor,
        Scenario::FilesScroll,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Scenario::IdleFull => "idle-full",
            Scenario::Drag => "drag",
            Scenario::Cursor => "cursor",
            Scenario::FilesScroll => "files-scroll",
        }
    }
}

/// Метрики одного сценария (все времена — миллисекунды).
#[derive(Clone, Debug)]
pub struct ScenarioMetrics {
    pub name: &'static str,
    pub frames: u64,
    pub frame_ms: Samples,
    pub render_ms: Samples,
    pub blend_ms: Samples,
    pub copy_ms: Samples,
    pub present_ms: Samples,
    pub shadow_ms: Samples,
    pub damage_rects_per_frame: f64,
    pub opaque_pct: f64,
    pub painted_px_per_frame: f64,
}

impl ScenarioMetrics {
    fn new(name: &'static str, capacity: usize) -> Self {
        Self {
            name,
            frames: 0,
            frame_ms: Samples::with_capacity(capacity),
            render_ms: Samples::with_capacity(capacity),
            blend_ms: Samples::with_capacity(capacity),
            copy_ms: Samples::with_capacity(capacity),
            present_ms: Samples::with_capacity(capacity),
            shadow_ms: Samples::with_capacity(capacity),
            damage_rects_per_frame: 0.0,
            opaque_pct: 0.0,
            painted_px_per_frame: 0.0,
        }
    }

    /// `frame - render - present`: диспетчеризация сервера, take damage, учёт.
    pub fn other_ms(&self) -> f64 {
        (self.frame_ms.avg() - self.render_ms.avg() - self.present_ms.avg()).max(0.0)
    }

    /// Строка для serial-лога (её же грепает e2e).
    pub fn log_line(&self) -> String {
        format!(
            "pagh-desktop: frametime scenario={} frames={} frame_ms_avg={:.3} p50={:.3} p95={:.3} max={:.3} render_ms={:.3} blend_ms={:.3} shadow_ms={:.3} copy_ms={:.3} present_ms={:.3} other_ms={:.3} damage_rects={:.2} opaque_pct={:.1} painted_px={:.0}",
            self.name,
            self.frames,
            self.frame_ms.avg(),
            self.frame_ms.p50(),
            self.frame_ms.p95(),
            self.frame_ms.max(),
            self.render_ms.avg(),
            self.blend_ms.avg(),
            self.shadow_ms.avg(),
            self.copy_ms.avg(),
            self.present_ms.avg(),
            self.other_ms(),
            self.damage_rects_per_frame,
            self.opaque_pct,
            self.painted_px_per_frame,
        )
    }

    fn json(&self) -> String {
        let stats = |samples: &Samples| {
            format!(
                "{{\"avg\":{:.3},\"p50\":{:.3},\"p95\":{:.3},\"max\":{:.3}}}",
                samples.avg(),
                samples.p50(),
                samples.p95(),
                samples.max()
            )
        };
        format!(
            "    {{\"name\":\"{}\",\"frames\":{},\"frame_ms\":{},\"render_ms\":{},\"blend_ms\":{},\"shadow_ms\":{},\"copy_ms\":{},\"present_ms\":{},\"other_ms\":{:.3},\"damage_rects_per_frame\":{:.2},\"opaque_pct\":{:.1},\"painted_px_per_frame\":{:.0}}}",
            self.name,
            self.frames,
            stats(&self.frame_ms),
            stats(&self.render_ms),
            stats(&self.blend_ms),
            stats(&self.shadow_ms),
            stats(&self.copy_ms),
            stats(&self.present_ms),
            self.other_ms(),
            self.damage_rects_per_frame,
            self.opaque_pct,
            self.painted_px_per_frame,
        )
    }
}

/// Итог прогона: контекст + метрики всех сценариев.
#[derive(Clone, Debug)]
pub struct BenchReport {
    pub label: String,
    pub backend: &'static str,
    pub screen: (u32, u32),
    pub frames_per_scenario: u64,
    pub scenarios: Vec<ScenarioMetrics>,
    pub generated_unix: u64,
}

impl BenchReport {
    /// Напечатать заголовок и по строке на сценарий (serial в QEMU).
    pub fn print(&self) {
        println!(
            "pagh-desktop: frametime harness label={} backend={} screen={}x{} frames={} clock={} cycles_per_ns={:.3}",
            self.label,
            self.backend,
            self.screen.0,
            self.screen.1,
            self.frames_per_scenario,
            timing::clock_source(),
            timing::cycles_per_ns()
        );
        for scenario in &self.scenarios {
            println!("{}", scenario.log_line());
        }
    }

    /// JSON без serde: поля фиксированы, имена сценариев — ASCII-литералы.
    pub fn to_json(&self) -> String {
        let mut body = String::new();
        body.push_str("{\n");
        body.push_str("  \"schema\": \"pagh-frametime/1\",\n");
        body.push_str(&format!("  \"label\": \"{}\",\n", self.label));
        body.push_str(&format!("  \"generated_unix\": {},\n", self.generated_unix));
        body.push_str(&format!("  \"clock\": \"{}\",\n", timing::clock_source()));
        body.push_str(&format!(
            "  \"cycles_per_ns\": {:.4},\n",
            timing::cycles_per_ns()
        ));
        body.push_str(&format!("  \"backend\": \"{}\",\n", self.backend));
        body.push_str(&format!(
            "  \"screen\": \"{}x{}\",\n",
            self.screen.0, self.screen.1
        ));
        body.push_str(&format!(
            "  \"frames_per_scenario\": {},\n",
            self.frames_per_scenario
        ));
        body.push_str("  \"scenarios\": [\n");
        let rows: Vec<String> = self.scenarios.iter().map(|s| s.json()).collect();
        body.push_str(&rows.join(",\n"));
        body.push_str("\n  ]\n}\n");
        body
    }
}

/// Backend для хоста: тот же blit в `FbLayout` XRGB8888, но в обычный `Vec`
/// (на хосте нет `/dev/fb0`, а headless-backend не копирует ни одного байта).
pub struct MemoryBackend {
    layout: FbLayout,
    buf: Vec<u8>,
    frames: u64,
}

impl MemoryBackend {
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width.max(1) * 4;
        let smem_len = stride * height.max(1);
        let layout = FbLayout {
            width,
            height,
            bpp: 32,
            stride,
            smem_len,
            red: BitField {
                offset: 16,
                length: 8,
            },
            green: BitField {
                offset: 8,
                length: 8,
            },
            blue: BitField {
                offset: 0,
                length: 8,
            },
        };
        Self {
            layout,
            buf: vec![0u8; smem_len as usize],
            frames: 0,
        }
    }

    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }
}

impl Backend for MemoryBackend {
    fn size(&self) -> (u32, u32) {
        (self.layout.width, self.layout.height)
    }

    fn present(&mut self, canvas: &crate::canvas::Canvas, damage: &[Rect]) -> io::Result<()> {
        self.layout.blit_damage(&mut self.buf, canvas, damage);
        self.frames += 1;
        Ok(())
    }

    fn frames(&self) -> u64 {
        self.frames
    }

    fn kind(&self) -> crate::backend::BackendKind {
        crate::backend::BackendKind::Headless
    }
}

/// Собрать рабочий стол для сценария: Terminal + Files, без AF_UNIX-клиентов.
///
/// Публична, потому что пиксельные goldens (`tests/render_goldens.rs`) обязаны
/// снимать ровно ту же сцену, на которой меряется frame time.
pub fn build_desktop(width: u32, height: u32) -> Compositor {
    let mut compositor = Compositor::new(width, height);
    compositor.set_clock("23:38", "15.11.2026");
    let (term_w, term_h) = (640u32, 400u32);
    let (files_w, files_h) = (560u32, 340u32);
    let terminal_state = DemoState {
        focused: true,
        mouse: Some(Point::new(280, 160)),
        last_input: "move 280,160".into(),
        ..DemoState::default()
    };
    let files_state = DemoState {
        focused: false,
        ..DemoState::default()
    };
    // Окна создаются и позиционируются до первого кадра, чтобы damage от
    // `add_window` ушёл в прогрев, а не в первую измеряемую выборку.
    let _ = compositor.add_window(
        1,
        1,
        "Terminal - pagh",
        term_w,
        term_h,
        PixelFormat::Xrgb8888,
    );
    let _ = compositor.commit_surface(
        1,
        1,
        demo::terminal_surface(term_w, term_h, &terminal_state),
    );
    let _ = compositor.add_window(
        2,
        2,
        "Files - /mnt",
        files_w,
        files_h,
        PixelFormat::Xrgb8888,
    );
    let _ = compositor.commit_surface(2, 2, demo::files_surface(files_w, files_h, &files_state));
    // Раскладка как в scenario.rs: Terminal слева, Files справа ниже.
    let term_at = Point::new(60, 70);
    let files_at = Point::new(
        (width as i32 - files_w as i32 - 40).max(10),
        (height as i32 - files_h as i32 - 120).max(10),
    );
    compositor.move_window(1, term_at);
    compositor.move_window(2, files_at);
    let _ = compositor.focus_and_raise(1);
    let _ = compositor.pointer_motion(Point::new(
        (width as i32 / 2).min(width as i32 - 2),
        (height as i32 / 2).min(height as i32 - 2),
    ));
    compositor
}

/// Подготовить сценарий к кадрам (клик/drag-состояние, положения указателя).
fn prime(compositor: &mut Compositor, scenario: Scenario) {
    match scenario {
        Scenario::IdleFull => {
            let _ = compositor.pointer_motion(Point::new(960, 620));
        }
        Scenario::Drag => {
            // Заголовок Terminal: окно в (60,70), шапка ~28 px.
            let start = Point::new(200, 84);
            let _ = compositor.pointer_motion(start);
            let _ = compositor.pointer_button(MouseButton::Left, true);
        }
        Scenario::Cursor => {
            let _ = compositor.pointer_motion(Point::new(640, 400));
        }
        Scenario::FilesScroll => {
            let _ = compositor.pointer_motion(Point::new(900, 430));
        }
    }
}

/// Один кадр сценария: сдвинуть состояние, отрисовать damage, презентовать.
fn drive_frame(
    compositor: &mut Compositor,
    scenario: Scenario,
    index: usize,
    files_size: (u32, u32),
) {
    match scenario {
        Scenario::IdleFull => {}
        Scenario::Drag => {
            // Треугольная волна ±20 px: окно реально едет, damage — два ректа.
            let phase = (index % 40) as i32 - 20;
            let (x, y) = (200 + phase, 84 + phase / 2);
            let _ = compositor.pointer_motion(Point::new(x, y));
        }
        Scenario::Cursor => {
            // Круг радиусом 300 px: курсор проходит над окнами и обоями
            // (hover-состояния заголовков тоже перерисовываются).
            let step = (index as f64) * 0.07;
            let (cx, cy) = (640.0, 400.0);
            let x = cx + 300.0 * step.cos();
            let y = cy + 220.0 * step.sin();
            let _ = compositor.pointer_motion(Point::new(x as i32, y as i32));
        }
        Scenario::FilesScroll => {
            let state = DemoState {
                focused: false,
                scroll: (index as i32 * 24) % 960,
                ..DemoState::default()
            };
            let _ = compositor.commit_surface(
                2,
                2,
                demo::files_surface(files_size.0, files_size.1, &state),
            );
        }
    }
}

/// Один измеренный кадр: `render` + `present`, счётчики стадий снимаются сразу
/// после present. Возвращает (frame_ms, damage_rects, painted_px).
fn measured_frame(
    compositor: &mut Compositor,
    backend: &mut dyn Backend,
    meters: &mut ScenarioMetrics,
    full: bool,
) -> io::Result<()> {
    // Счётчики стадий обнуляются здесь, а не до `drive_frame`: подготовка
    // клиентского кадра (например, `demo::files_surface`) — не работа
    // композитора и не должна попадать в breakdown.
    timing::reset_frame();
    let start = timing::now_cycles();
    let damage = if full {
        compositor.render_full()
    } else {
        compositor.render()
    };
    {
        // `Present` включает `Copy`: так же размечен серверный путь.
        let _stage = timing::Scope::new(crate::timing::Stage::Present);
        backend.present(compositor.frame(), &damage)?;
    }
    let total = timing::now_cycles().saturating_sub(start);
    let stages = timing::take_frame();
    meters.frame_ms.push(timing::cycles_to_ms(total));
    meters.render_ms.push(stages.render_ms());
    meters.blend_ms.push(stages.blend_ms());
    meters.copy_ms.push(stages.copy_ms());
    meters.present_ms.push(stages.present_ms());
    meters.shadow_ms.push(stages.shadow_ms());
    meters.frames += 1;
    meters.damage_rects_per_frame += damage.len() as f64;
    let painted = stages.opaque_px + stages.blended_px;
    meters.painted_px_per_frame += painted as f64;
    meters.opaque_pct += if painted == 0 {
        0.0
    } else {
        stages.opaque_px as f64 * 100.0 / painted as f64
    };
    Ok(())
}

/// Прогнать все сценарии на backend'е. `label` попадает в отчёт (`host`/`qemu`).
pub fn run(backend: &mut dyn Backend, frames: usize, label: &str) -> io::Result<BenchReport> {
    let (width, height) = match backend.size() {
        (0, _) | (_, 0) => DEFAULT_SCREEN,
        size => size,
    };
    let mut scenarios = Vec::with_capacity(Scenario::ALL.len());
    for scenario in Scenario::ALL {
        let mut compositor = build_desktop(width, height);
        prime(&mut compositor, scenario);
        let files_size = (560u32.min(width), 340u32.min(height));
        // Прогрев: кэши обоев/панели/glyph и стабилизация hover.
        let full = scenario == Scenario::IdleFull;
        for index in 0..WARMUP_FRAMES {
            drive_frame(&mut compositor, scenario, index, files_size);
            let damage = if full {
                compositor.render_full()
            } else {
                compositor.render()
            };
            backend.present(compositor.frame(), &damage)?;
        }
        let mut meters = ScenarioMetrics::new(scenario.id(), frames);
        for index in WARMUP_FRAMES..WARMUP_FRAMES + frames {
            drive_frame(&mut compositor, scenario, index, files_size);
            measured_frame(&mut compositor, backend, &mut meters, full)?;
        }
        let count = meters.frames.max(1) as f64;
        meters.damage_rects_per_frame /= count;
        meters.opaque_pct /= count;
        meters.painted_px_per_frame /= count;
        scenarios.push(meters);
    }
    Ok(BenchReport {
        label: label.to_string(),
        backend: backend.kind().name(),
        screen: (width, height),
        frames_per_scenario: frames as u64,
        scenarios,
        generated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_blits_damage_into_its_buffer() {
        let mut backend = MemoryBackend::new(8, 6);
        let canvas = crate::canvas::Canvas::new(8, 6, crate::color::rgb(0x10, 0x20, 0x30));
        backend
            .present(&canvas, &[Rect::new(0, 0, 8, 6)])
            .expect("present");
        assert_eq!(backend.frames(), 1);
        // XRGB8888: alpha-байт нулевой, каналы — на своих местах.
        assert_eq!(&backend.buffer()[..4], &[0x30, 0x20, 0x10, 0x00]);
    }

    #[test]
    fn every_scenario_produces_frames_and_damage() {
        let mut backend = MemoryBackend::new(640, 480);
        let report = run(&mut backend, 6, "test").expect("bench");
        assert_eq!(report.scenarios.len(), Scenario::ALL.len());
        for scenario in &report.scenarios {
            assert_eq!(scenario.frames, 6, "{}: кадры", scenario.name);
            assert!(
                scenario.damage_rects_per_frame > 0.0,
                "{}: damage должен быть ненулевым",
                scenario.name
            );
            assert!(scenario.frame_ms.avg() > 0.0, "{}: время", scenario.name);
            assert!(
                scenario.render_ms.avg() > 0.0,
                "{}: render-стадия",
                scenario.name
            );
            // Стадии между собой здесь НЕ сравниваем: счётчики в `timing`
            // глобальные (TLS нельзя — см. `timing.rs`), а параллельные тесты
            // одного процесса рисуют свои холсты и добавляют такты в те же
            // счётчики. В реальном бинаре путь однопоточный, и это точно.
            assert!(
                scenario.blend_ms.avg() <= scenario.frame_ms.avg() + 60_000.0,
                "{}: blend неправдоподобно велик: {}",
                scenario.name,
                scenario.blend_ms.avg()
            );
            assert!(scenario.opaque_pct >= 0.0 && scenario.opaque_pct <= 100.0);
        }
    }

    #[test]
    fn report_json_and_log_lines_are_machine_readable() {
        let mut backend = MemoryBackend::new(320, 240);
        let report = run(&mut backend, 3, "json-test").expect("bench");
        let json = report.to_json();
        assert!(json.starts_with("{\n  \"schema\": \"pagh-frametime/1\""));
        assert!(json.contains("\"name\":\"drag\""));
        assert!(json.contains("\"frame_ms\":{\"avg\":"));
        assert!(
            json.matches("\"p50\"").count() >= 4 * 5,
            "каждый сценарий несёт p50 по пяти выборкам"
        );
        for scenario in &report.scenarios {
            let line = scenario.log_line();
            assert!(line.starts_with("pagh-desktop: frametime scenario="));
            assert!(line.contains("frame_ms_avg="));
            assert!(line.contains("copy_ms="));
        }
    }
}
