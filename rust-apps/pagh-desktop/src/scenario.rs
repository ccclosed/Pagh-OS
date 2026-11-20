//! Воспроизводимый headless-сценарий для скриншотов: два окна, фокус, drag,
//! контекстное меню, запуск приложения, network/calendar flyout, maximize.
//!
//! Результат — PNG-файлы в `rust-apps/pagh-desktop/screenshots/`, которые
//! коммитятся как визуальное evidence вертикального среза.

use crate::compositor::{Compositor, HostEvent, MouseButton};
use crate::demo::{self, DemoState};
use crate::geometry::Point;
use crate::png;
use crate::surface::PixelFormat;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Итог сценария.
#[derive(Clone, Debug)]
pub struct ScenarioReport {
    pub out_dir: PathBuf,
    pub screenshots: Vec<PathBuf>,
    pub notes: Vec<String>,
}

impl ScenarioReport {
    pub fn write_transcript(&self) -> io::Result<PathBuf> {
        let path = self.out_dir.join("scenario.txt");
        let mut body = String::new();
        body.push_str("Pagh Desktop headless scenario (PDP/1)\n");
        body.push_str("screen: 1280x720, panel 44 px, clock 23:38 / 15.11.2026\n\n");
        for note in &self.notes {
            body.push_str("- ");
            body.push_str(note);
            body.push('\n');
        }
        body.push_str("\nscreenshots:\n");
        for path in &self.screenshots {
            body.push_str("- ");
            body.push_str(&path.file_name().unwrap_or_default().to_string_lossy());
            body.push('\n');
        }
        fs::write(&path, body)?;
        Ok(path)
    }
}

/// Проиграть сценарий и записать скриншоты.
pub fn run(out_dir: impl AsRef<Path>) -> io::Result<ScenarioReport> {
    let out_dir = out_dir.as_ref().to_path_buf();
    fs::create_dir_all(&out_dir)?;
    let mut compositor = Compositor::new(1280, 720);
    compositor.set_clock("23:38", "15.11.2026");
    let mut screenshots = Vec::new();
    let mut notes = Vec::new();

    // Окно Terminal.
    let mut terminal_state = DemoState {
        focused: true,
        mouse: Some(Point::new(300, 180)),
        last_input: "move 300,180".into(),
        ..DemoState::default()
    };
    compositor
        .add_window(1, 1, "Terminal - pagh", 640, 400, PixelFormat::Xrgb8888)
        .expect("окно терминала");
    compositor.move_window(1, Point::new(60, 70));
    compositor
        .commit_surface(1, 1, demo::terminal_surface(640, 400, &terminal_state))
        .expect("кадр терминала");

    // Окно Files.
    let mut files_state = DemoState {
        focused: false,
        ..DemoState::default()
    };
    compositor
        .add_window(2, 2, "Files - /mnt", 560, 340, PixelFormat::Xrgb8888)
        .expect("окно файлов");
    compositor.move_window(2, Point::new(680, 300));
    compositor
        .commit_surface(2, 2, demo::files_surface(560, 340, &files_state))
        .expect("кадр файлов");

    // Фокус на терминал (заголовок виден левее окна Files).
    click(&mut compositor, Point::new(120, 85));
    // Указатель — на свободные обои, чтобы не перекрывать заголовок и строки.
    compositor.pointer_motion(Point::new(900, 600));
    terminal_state.mouse = Some(Point::new(500, 300));
    terminal_state.last_input = "move 500,300".into();
    compositor
        .commit_surface(1, 1, demo::terminal_surface(640, 400, &terminal_state))
        .expect("кадр терминала");
    snap(
        &mut compositor,
        &out_dir,
        "01-desktop.png",
        &mut screenshots,
    )?;
    notes.push(
        "два окна, фокус на Terminal, панель 44px с часами 23:38 / 15.11.2026 и Wi-Fi".into(),
    );

    // Фокус на Files кликом по содержимому справа от терминала.
    files_state.mouse = Some(Point::new(450, 250));
    files_state.last_input = "move 450,250".into();
    compositor
        .commit_surface(2, 2, demo::files_surface(560, 340, &files_state))
        .expect("кадр файлов");
    click(&mut compositor, Point::new(900, 430));
    snap(
        &mut compositor,
        &out_dir,
        "02-focus-files.png",
        &mut screenshots,
    )?;
    notes.push("клик по содержимому Files: raise + focus, заголовок подсвечен".into());

    // Перетаскивание терминала за заголовок.
    compositor.pointer_motion(Point::new(200, 85));
    compositor.pointer_button(MouseButton::Left, true);
    compositor.pointer_motion(Point::new(300, 150));
    snap(
        &mut compositor,
        &out_dir,
        "03-drag-window.png",
        &mut screenshots,
    )?;
    compositor.pointer_button(MouseButton::Left, false);
    notes.push(
        "drag заголовка Terminal: окно следует за указателем, damage по старой/новой рамке".into(),
    );

    // Контекстное меню рабочего стола.
    right_click(&mut compositor, Point::new(980, 140));
    snap(
        &mut compositor,
        &out_dir,
        "04-context-menu.png",
        &mut screenshots,
    )?;
    notes.push("правый клик по рабочему столу: контекстное меню Open Terminal/Files/Paint…".into());

    // Пункт Open Terminal запускает новое окно.
    let menu_point = compositor
        .context_menu()
        .expect("меню открыто")
        .item_rect(0)
        .center();
    let events = click(&mut compositor, menu_point);
    let launched = events.iter().any(|event| {
        matches!(
            event,
            HostEvent::Launch(crate::compositor::AppKind::Terminal)
        )
    });
    assert!(launched, "пункт меню должен запросить запуск Terminal");

    let mut second_state = DemoState {
        focused: true,
        mouse: Some(Point::new(450, 260)),
        last_input: "move 450,260".into(),
        ..DemoState::default()
    };
    compositor
        .add_window(3, 3, "Terminal - /mnt", 600, 360, PixelFormat::Xrgb8888)
        .expect("второй терминал");
    compositor.move_window(3, Point::new(640, 230));
    compositor
        .commit_surface(3, 3, demo::terminal_surface(600, 360, &second_state))
        .expect("кадр второго терминала");
    snap(
        &mut compositor,
        &out_dir,
        "05-launched-terminal.png",
        &mut screenshots,
    )?;
    notes.push("Open Terminal: сервер отдал Launch(Terminal), появилось третье окно".into());

    // Network flyout.
    let network = crate::panel::network_rect(compositor.screen()).center();
    click(&mut compositor, network);
    snap(
        &mut compositor,
        &out_dir,
        "06-network-flyout.png",
        &mut screenshots,
    )?;
    notes.push(
        "клик по иконке сети: флайаут «Сети» — статус и карточка проводного подключения; \
         IP и скорости не рисуются, пока ядро их не отдаёт"
            .into(),
    );

    // Calendar flyout.
    click(&mut compositor, Point::new(300, 600));
    let clock = crate::panel::clock_rect(compositor.screen()).center();
    click(&mut compositor, clock);
    snap(
        &mut compositor,
        &out_dir,
        "07-calendar.png",
        &mut screenshots,
    )?;
    notes.push("клик по часам: календарь November 2026, 15-е выделено".into());

    // Maximize третьего окна.
    click(&mut compositor, Point::new(300, 600));
    let titlebar = compositor.window(3).expect("окно 3").titlebar_rect();
    click(
        &mut compositor,
        Point::new(titlebar.x + 40, titlebar.center().y),
    );
    let maximize = compositor
        .window(3)
        .expect("окно 3")
        .maximize_rect()
        .center();
    click(&mut compositor, maximize);
    debug_assert!(compositor.window(3).expect("окно 3").maximized);
    // Клиент получил configure и присылает кадр под новый размер (как настоящий клиент).
    let maximized_content = compositor.window(3).expect("окно 3").content_size;
    second_state.last_input = format!("configure {}x{}", maximized_content.w, maximized_content.h);
    compositor
        .commit_surface(
            3,
            3,
            demo::terminal_surface(maximized_content.w, maximized_content.h, &second_state),
        )
        .expect("кадр максимизированного терминала");
    snap(
        &mut compositor,
        &out_dir,
        "08-maximized.png",
        &mut screenshots,
    )?;
    notes.push("кнопка ^: окно развёрнуто над панелью, клиент прислал кадр под configure".into());

    // Финальный кадр с восстановленным окном для полноты.
    let restore = compositor
        .window(3)
        .expect("окно 3")
        .maximize_rect()
        .center();
    click(&mut compositor, restore);
    debug_assert!(!compositor.window(3).expect("окно 3").maximized);
    second_state.last_input = "restored 600x360".into();
    compositor
        .commit_surface(3, 3, demo::terminal_surface(600, 360, &second_state))
        .expect("кадр восстановленного терминала");
    snap(
        &mut compositor,
        &out_dir,
        "09-restored.png",
        &mut screenshots,
    )?;
    notes.push("повторный клик ^: окно восстановлено в прежнюю геометрию".into());

    let report = ScenarioReport {
        out_dir,
        screenshots,
        notes,
    };
    report.write_transcript()?;
    Ok(report)
}

fn click(compositor: &mut Compositor, point: Point) -> Vec<HostEvent> {
    compositor.pointer_motion(point);
    let mut events = compositor.pointer_button(MouseButton::Left, true);
    events.extend(compositor.pointer_button(MouseButton::Left, false));
    events
}

fn right_click(compositor: &mut Compositor, point: Point) {
    compositor.pointer_motion(point);
    compositor.pointer_button(MouseButton::Right, true);
    compositor.pointer_button(MouseButton::Right, false);
}

fn snap(
    compositor: &mut Compositor,
    out_dir: &Path,
    name: &str,
    screenshots: &mut Vec<PathBuf>,
) -> io::Result<()> {
    compositor.render_full();
    let path = out_dir.join(name);
    png::write_png(&path, compositor.frame())?;
    screenshots.push(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_produces_screenshots() {
        let dir = std::env::temp_dir().join(format!("pagh-scenario-{}", std::process::id()));
        let report = run(&dir).unwrap();
        assert_eq!(report.screenshots.len(), 9);
        for path in &report.screenshots {
            let data = fs::read(path).unwrap();
            assert!(data.starts_with(b"\x89PNG"), "{}", path.display());
            assert!(data.len() > 10_000, "{}", path.display());
        }
        assert!(dir.join("scenario.txt").exists());
        fs::remove_dir_all(dir).ok();
    }
}
