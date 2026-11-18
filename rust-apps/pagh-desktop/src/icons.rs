//! Готовые иконки Fluent UI System Icons (MIT), запечённые в бинарник.
//!
//! Ассеты — чёрные regular-глифы (`#000000` + альфа) из
//! `assets/icons/`: при отрисовке RGB заменяется цветом темы, альфа
//! сохраняется, поэтому один файл годится и для обычного, и для акцентного,
//! и для приглушённого состояния. Filled-варианты намеренно не используются:
//! они приходят как `#212121` и на тёмной панели невидимы без той же
//! перекраски, которую regular уже даёт даром.
//!
//! PNG разбирается лениво и кэшируется по `(иконка, размер)`: каждый ассет
//! декодируется не больше одного раза за процесс. Декодер — `crate::png`
//! (собственный inflate в `crate::deflate`), без внешних зависимостей, чтобы
//! статическая musl-сборка оставалась самодостаточной.
//!
//! Лицензия и атрибуция: `assets/icons/LICENSE`, раздел «Иконки» в README.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::canvas::Canvas;
use crate::color::{self, Color};
use crate::geometry::{Point, Rect};

/// Доступные размеры ассетов.
pub const SIZES: [u32; 4] = [16, 20, 24, 32];

/// Иконки, подготовленные для десктопа.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// Лаунчер: сетка приложений.
    Grid,
    /// Терминал.
    Terminal,
    /// Файлы/каталог.
    Folder,
    /// Графический редактор (картинка).
    Image,
    /// Проводная сеть: вилка с кабелем (Plug Connected).
    NetworkWired,
    /// Свернуть окно.
    WindowMinimize,
    /// Развернуть окно.
    WindowMaximize,
    /// Закрыть окно.
    WindowClose,
}

impl Icon {
    pub const ALL: [Icon; 8] = [
        Icon::Grid,
        Icon::Terminal,
        Icon::Folder,
        Icon::Image,
        Icon::NetworkWired,
        Icon::WindowMinimize,
        Icon::WindowMaximize,
        Icon::WindowClose,
    ];

    /// Имя глифа Fluent — для отчётов и атрибуции.
    pub fn fluent_name(self) -> &'static str {
        match self {
            Self::Grid => "Grid",
            Self::Terminal => "Window Console",
            Self::Folder => "Folder",
            Self::Image => "Image",
            Self::NetworkWired => "Plug Connected",
            Self::WindowMinimize => "Subtract",
            Self::WindowMaximize => "Square",
            Self::WindowClose => "Dismiss",
        }
    }

    /// PNG-байты ассета; `None` — для этого размера файла нет.
    pub fn png(self, size: u32) -> Option<&'static [u8]> {
        Some(match (self, size) {
            (Self::Grid, 16) => include_bytes!("../assets/icons/grid-16.png"),
            (Self::Grid, 20) => include_bytes!("../assets/icons/grid-20.png"),
            (Self::Grid, 24) => include_bytes!("../assets/icons/grid-24.png"),
            (Self::Grid, 32) => include_bytes!("../assets/icons/grid-32.png"),
            (Self::Terminal, 16) => include_bytes!("../assets/icons/terminal-16.png"),
            (Self::Terminal, 20) => include_bytes!("../assets/icons/terminal-20.png"),
            (Self::Terminal, 24) => include_bytes!("../assets/icons/terminal-24.png"),
            (Self::Terminal, 32) => include_bytes!("../assets/icons/terminal-32.png"),
            (Self::Folder, 16) => include_bytes!("../assets/icons/folder-16.png"),
            (Self::Folder, 20) => include_bytes!("../assets/icons/folder-20.png"),
            (Self::Folder, 24) => include_bytes!("../assets/icons/folder-24.png"),
            (Self::Folder, 32) => include_bytes!("../assets/icons/folder-32.png"),
            (Self::Image, 16) => include_bytes!("../assets/icons/image-16.png"),
            (Self::Image, 20) => include_bytes!("../assets/icons/image-20.png"),
            (Self::Image, 24) => include_bytes!("../assets/icons/image-24.png"),
            (Self::Image, 32) => include_bytes!("../assets/icons/image-32.png"),
            (Self::NetworkWired, 16) => include_bytes!("../assets/icons/network-wired-16.png"),
            (Self::NetworkWired, 20) => include_bytes!("../assets/icons/network-wired-20.png"),
            (Self::NetworkWired, 24) => include_bytes!("../assets/icons/network-wired-24.png"),
            (Self::NetworkWired, 32) => include_bytes!("../assets/icons/network-wired-32.png"),
            (Self::WindowMinimize, 16) => include_bytes!("../assets/icons/window-minimize-16.png"),
            (Self::WindowMinimize, 20) => include_bytes!("../assets/icons/window-minimize-20.png"),
            (Self::WindowMinimize, 24) => include_bytes!("../assets/icons/window-minimize-24.png"),
            (Self::WindowMinimize, 32) => include_bytes!("../assets/icons/window-minimize-32.png"),
            (Self::WindowMaximize, 16) => include_bytes!("../assets/icons/window-maximize-16.png"),
            (Self::WindowMaximize, 20) => include_bytes!("../assets/icons/window-maximize-20.png"),
            (Self::WindowMaximize, 24) => include_bytes!("../assets/icons/window-maximize-24.png"),
            (Self::WindowMaximize, 32) => include_bytes!("../assets/icons/window-maximize-32.png"),
            (Self::WindowClose, 16) => include_bytes!("../assets/icons/window-close-16.png"),
            (Self::WindowClose, 20) => include_bytes!("../assets/icons/window-close-20.png"),
            (Self::WindowClose, 24) => include_bytes!("../assets/icons/window-close-24.png"),
            (Self::WindowClose, 32) => include_bytes!("../assets/icons/window-close-32.png"),
            _ => return None,
        })
    }
}

/// Декодированная маска иконки: RGBA8, RGB чёрный (перекрашивается при blit).
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Raster {
    /// Альфа пикселя; за границами — 0.
    pub fn alpha(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.rgba[((y * self.width + x) as usize) * 4 + 3]
    }
}

type Cache = Mutex<HashMap<(Icon, u32), Arc<Raster>>>;

static CACHE: OnceLock<Cache> = OnceLock::new();

/// Декодированный ассет из кэша (декодируется при первом обращении).
pub fn raster(icon: Icon, size: u32) -> Option<Arc<Raster>> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = guard.get(&(icon, size)) {
        return Some(Arc::clone(cached));
    }
    let decoded = crate::png::decode_png(icon.png(size)?)?;
    let raster = Arc::new(Raster {
        width: decoded.width,
        height: decoded.height,
        rgba: decoded.rgba,
    });
    guard.insert((icon, size), Arc::clone(&raster));
    Some(raster)
}

/// Нарисовать иконку по центру `center`, перекрасив её в `tint`.
///
/// `opacity` масштабирует альфу (приглушение без смены цвета). Недоступный
/// ассет — тихий no-op: десктоп не должен падать из-за иконки.
pub fn draw(canvas: &mut Canvas, icon: Icon, size: u32, center: Point, tint: Color, opacity: u8) {
    let Some(raster) = raster(icon, size) else {
        return;
    };
    let dst = Rect::new(
        center.x - raster.width as i32 / 2,
        center.y - raster.height as i32 / 2,
        raster.width as i32,
        raster.height as i32,
    );
    blit_tinted(canvas, &raster, dst, tint, opacity);
}

/// Наложить маску на холст: RGB источника заменяется `tint`, альфа сохраняется.
pub fn blit_tinted(canvas: &mut Canvas, raster: &Raster, dst: Rect, tint: Color, opacity: u8) {
    let (red, green, blue) = (color::red(tint), color::green(tint), color::blue(tint));
    for y in 0..raster.height as i32 {
        for x in 0..raster.width as i32 {
            let alpha = raster.alpha(x as u32, y as u32);
            if alpha == 0 {
                continue;
            }
            let alpha = ((u32::from(alpha) * u32::from(opacity) + 127) / 255) as u8;
            if alpha == 0 {
                continue;
            }
            canvas.blend(
                dst.x + x,
                dst.y + y,
                color::rgba(red, green, blue, alpha),
                255,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;

    #[test]
    fn every_asset_decodes_at_its_size() {
        for icon in Icon::ALL {
            for size in SIZES {
                let raster = raster(icon, size)
                    .unwrap_or_else(|| panic!("{} {size} не декодировался", icon.fluent_name()));
                assert_eq!((raster.width, raster.height), (size, size));
                let lit = (0..size * size)
                    .filter(|index| raster.rgba[*index as usize * 4 + 3] > 0)
                    .count();
                assert!(lit > 8, "{} {size}: пустая маска", icon.fluent_name());
            }
        }
        assert!(raster(Icon::Grid, 48).is_none(), "48 px не поставляется");
    }

    #[test]
    fn tint_replaces_rgb_and_keeps_alpha() {
        let icon = Icon::Folder;
        let size = 24;
        let source = raster(icon, size).expect("ассет");
        let tint = rgb(200, 40, 10);
        let mut canvas = Canvas::new(64, 64, rgb(0, 0, 0));
        draw(&mut canvas, icon, size, Point::new(32, 32), tint, 255);
        let origin = Point::new(32 - size as i32 / 2, 32 - size as i32 / 2);
        for y in 0..size {
            for x in 0..size {
                let alpha = source.alpha(x, y);
                let expected =
                    crate::color::blend(rgb(0, 0, 0), crate::color::rgba(200, 40, 10, alpha), 255);
                assert_eq!(
                    canvas.get(origin.x + x as i32, origin.y + y as i32),
                    expected,
                    "пиксель ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn opacity_scales_alpha() {
        let mut full = Canvas::new(64, 64, rgb(0, 0, 0));
        let mut half = Canvas::new(64, 64, rgb(0, 0, 0));
        let tint = rgb(255, 255, 255);
        draw(&mut full, Icon::Grid, 24, Point::new(32, 32), tint, 255);
        draw(&mut half, Icon::Grid, 24, Point::new(32, 32), tint, 128);
        let brightest = |canvas: &Canvas| {
            canvas
                .pixels()
                .iter()
                .map(|pixel| crate::color::red(*pixel))
                .max()
                .unwrap()
        };
        assert!(brightest(&half) < brightest(&full));
        assert!(brightest(&half) > 0);
    }

    #[test]
    fn cache_reuses_decoded_raster() {
        let first = raster(Icon::NetworkWired, 20).expect("ассет");
        let second = raster(Icon::NetworkWired, 20).expect("ассет");
        assert!(Arc::ptr_eq(&first, &second));
    }
}
