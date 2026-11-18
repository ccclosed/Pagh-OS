//! Готовые иконки Fluent UI System Icons (MIT), запечённые в бинарник.
//!
//! Ассеты — чёрные regular-глифы (`#000000` + альфа) из `assets/icons/`:
//! при отрисовке RGB заменяется цветом темы, альфа сохраняется, поэтому один
//! файл годится и для обычного, и для акцентного, и для приглушённого
//! состояния. Filled-варианты намеренно не используются: они приходят как
//! `#212121` и на тёмном фоне невидимы без той же перекраски.
//!
//! PNG разбирается лениво и кэшируется по `(иконка, размер)`: каждый ассет
//! декодируется не больше одного раза за процесс. Декодер — [`crate::png`]
//! поверх собственного inflate в [`crate::deflate`]; внешних зависимостей
//! нет, статическая musl-сборка остаётся самодостаточной.
//!
//! Лицензия и атрибуция: `assets/icons/LICENSE`, `assets/icons/README.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use pagh_ui::canvas::Canvas;
use pagh_ui::color::Color;
use pagh_ui::geom::{Point, Rect};

/// Доступные размеры ассетов.
pub const SIZES: [u32; 4] = [16, 20, 24, 32];

/// Иконки, подготовленные для Files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FluentIcon {
    /// Каталог.
    Folder,
    /// Каталог с содержимым (пункт «Документы»).
    FolderOpen,
    /// Обычный файл (Document Text).
    Document,
    /// Домашний каталог.
    Home,
    /// Рабочий стол.
    Desktop,
    /// Загрузки (Arrow Download).
    Download,
    /// Изображение.
    Image,
    /// Аудио (Music Note 2).
    MusicNote,
    /// Видео (Movies and TV).
    Video,
    /// Корзина (Delete).
    Delete,
    /// Устройство/раздел (Hard Drive).
    Drive,
    /// Сеть (Globe).
    Network,
}

impl FluentIcon {
    pub const ALL: [FluentIcon; 12] = [
        FluentIcon::Folder,
        FluentIcon::FolderOpen,
        FluentIcon::Document,
        FluentIcon::Home,
        FluentIcon::Desktop,
        FluentIcon::Download,
        FluentIcon::Image,
        FluentIcon::MusicNote,
        FluentIcon::Video,
        FluentIcon::Delete,
        FluentIcon::Drive,
        FluentIcon::Network,
    ];

    /// Имя глифа Fluent — для отчётов и атрибуции.
    pub fn fluent_name(self) -> &'static str {
        match self {
            FluentIcon::Folder => "Folder",
            FluentIcon::FolderOpen => "Folder Open",
            FluentIcon::Document => "Document Text",
            FluentIcon::Home => "Home",
            FluentIcon::Desktop => "Desktop",
            FluentIcon::Download => "Arrow Download",
            FluentIcon::Image => "Image",
            FluentIcon::MusicNote => "Music Note 2",
            FluentIcon::Video => "Movies and TV",
            FluentIcon::Delete => "Delete",
            FluentIcon::Drive => "Hard Drive",
            FluentIcon::Network => "Globe",
        }
    }

    /// PNG-ассет нужного размера (`None` — такого размера нет).
    pub fn png(self, size: u32) -> Option<&'static [u8]> {
        Some(match (self, size) {
            (FluentIcon::Folder, 16) => include_bytes!("../assets/icons/folder-16.png"),
            (FluentIcon::Folder, 20) => include_bytes!("../assets/icons/folder-20.png"),
            (FluentIcon::Folder, 24) => include_bytes!("../assets/icons/folder-24.png"),
            (FluentIcon::Folder, 32) => include_bytes!("../assets/icons/folder-32.png"),
            (FluentIcon::FolderOpen, 16) => include_bytes!("../assets/icons/folder-open-16.png"),
            (FluentIcon::FolderOpen, 20) => include_bytes!("../assets/icons/folder-open-20.png"),
            (FluentIcon::FolderOpen, 24) => include_bytes!("../assets/icons/folder-open-24.png"),
            (FluentIcon::FolderOpen, 32) => include_bytes!("../assets/icons/folder-open-32.png"),
            (FluentIcon::Document, 16) => include_bytes!("../assets/icons/document-16.png"),
            (FluentIcon::Document, 20) => include_bytes!("../assets/icons/document-20.png"),
            (FluentIcon::Document, 24) => include_bytes!("../assets/icons/document-24.png"),
            (FluentIcon::Document, 32) => include_bytes!("../assets/icons/document-32.png"),
            (FluentIcon::Home, 16) => include_bytes!("../assets/icons/home-16.png"),
            (FluentIcon::Home, 20) => include_bytes!("../assets/icons/home-20.png"),
            (FluentIcon::Home, 24) => include_bytes!("../assets/icons/home-24.png"),
            (FluentIcon::Home, 32) => include_bytes!("../assets/icons/home-32.png"),
            (FluentIcon::Desktop, 16) => include_bytes!("../assets/icons/desktop-16.png"),
            (FluentIcon::Desktop, 20) => include_bytes!("../assets/icons/desktop-20.png"),
            (FluentIcon::Desktop, 24) => include_bytes!("../assets/icons/desktop-24.png"),
            (FluentIcon::Desktop, 32) => include_bytes!("../assets/icons/desktop-32.png"),
            (FluentIcon::Download, 16) => include_bytes!("../assets/icons/arrow-download-16.png"),
            (FluentIcon::Download, 20) => include_bytes!("../assets/icons/arrow-download-20.png"),
            (FluentIcon::Download, 24) => include_bytes!("../assets/icons/arrow-download-24.png"),
            (FluentIcon::Download, 32) => include_bytes!("../assets/icons/arrow-download-32.png"),
            (FluentIcon::Image, 16) => include_bytes!("../assets/icons/image-16.png"),
            (FluentIcon::Image, 20) => include_bytes!("../assets/icons/image-20.png"),
            (FluentIcon::Image, 24) => include_bytes!("../assets/icons/image-24.png"),
            (FluentIcon::Image, 32) => include_bytes!("../assets/icons/image-32.png"),
            (FluentIcon::MusicNote, 16) => include_bytes!("../assets/icons/music-note-16.png"),
            (FluentIcon::MusicNote, 20) => include_bytes!("../assets/icons/music-note-20.png"),
            (FluentIcon::MusicNote, 24) => include_bytes!("../assets/icons/music-note-24.png"),
            (FluentIcon::MusicNote, 32) => include_bytes!("../assets/icons/music-note-32.png"),
            (FluentIcon::Video, 16) => include_bytes!("../assets/icons/video-16.png"),
            (FluentIcon::Video, 20) => include_bytes!("../assets/icons/video-20.png"),
            (FluentIcon::Video, 24) => include_bytes!("../assets/icons/video-24.png"),
            (FluentIcon::Video, 32) => include_bytes!("../assets/icons/video-32.png"),
            (FluentIcon::Delete, 16) => include_bytes!("../assets/icons/delete-16.png"),
            (FluentIcon::Delete, 20) => include_bytes!("../assets/icons/delete-20.png"),
            (FluentIcon::Delete, 24) => include_bytes!("../assets/icons/delete-24.png"),
            (FluentIcon::Delete, 32) => include_bytes!("../assets/icons/delete-32.png"),
            (FluentIcon::Drive, 16) => include_bytes!("../assets/icons/drive-16.png"),
            (FluentIcon::Drive, 20) => include_bytes!("../assets/icons/drive-20.png"),
            (FluentIcon::Drive, 24) => include_bytes!("../assets/icons/drive-24.png"),
            (FluentIcon::Drive, 32) => include_bytes!("../assets/icons/drive-32.png"),
            (FluentIcon::Network, 16) => include_bytes!("../assets/icons/network-16.png"),
            (FluentIcon::Network, 20) => include_bytes!("../assets/icons/network-20.png"),
            (FluentIcon::Network, 24) => include_bytes!("../assets/icons/network-24.png"),
            (FluentIcon::Network, 32) => include_bytes!("../assets/icons/network-32.png"),
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

    /// Число непрозрачных пикселей — для тестов «иконка не пустая».
    pub fn lit_pixels(&self) -> usize {
        (0..self.width * self.height)
            .filter(|index| self.rgba[*index as usize * 4 + 3] > 0)
            .count()
    }
}

type Cache = Mutex<HashMap<(FluentIcon, u32), Arc<Raster>>>;

static CACHE: OnceLock<Cache> = OnceLock::new();

/// Декодированный ассет из кэша (декодируется при первом обращении).
pub fn raster(icon: FluentIcon, size: u32) -> Option<Arc<Raster>> {
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

/// Нарисовать иконку по центру `rect`, перекрасив её в `tint`.
///
/// `opacity` масштабирует альфу (приглушение без смены цвета). Недоступный
/// ассет — тихий no-op: окно не должно падать из-за иконки.
pub fn draw(
    canvas: &mut Canvas,
    icon: FluentIcon,
    size: u32,
    rect: Rect,
    tint: Color,
    opacity: u8,
) {
    let Some(raster) = raster(icon, size) else {
        return;
    };
    let dst = Rect::new(
        rect.x + (rect.w - raster.width as i32) / 2,
        rect.y + (rect.h - raster.height as i32) / 2,
        raster.width as i32,
        raster.height as i32,
    );
    blit_tinted(canvas, &raster, dst, tint, opacity);
}

/// Наложить маску на холст: RGB источника заменяется `tint`, альфа сохраняется.
pub fn blit_tinted(canvas: &mut Canvas, raster: &Raster, dst: Rect, tint: Color, opacity: u8) {
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
            canvas.blend_pixel(
                Point::new(dst.x + x, dst.y + y),
                Color::rgba(tint.r, tint.g, tint.b, alpha),
            );
        }
    }
}

/// Размер маски, которому соответствует прямоугольник иконки.
///
/// Сайдбар и компактный список рисуют 14 px, сетка — 32 px. Нативные 16 px
/// слегка перекрывают 14-пиксельный прямоугольник (1 px с каждой стороны) —
/// это лучше, чем размытая интерполяция 16→14.
pub fn best_size(rect: Rect) -> u32 {
    let target = rect.w.max(rect.h);
    *SIZES
        .iter()
        .find(|size| **size as i32 >= target)
        .unwrap_or(&32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pagh_ui::color::Color;

    #[test]
    fn every_asset_decodes_at_its_size() {
        for icon in FluentIcon::ALL {
            for size in SIZES {
                let raster = raster(icon, size)
                    .unwrap_or_else(|| panic!("{} {size} не декодировался", icon.fluent_name()));
                assert_eq!((raster.width, raster.height), (size, size));
                assert!(
                    raster.lit_pixels() > 8,
                    "{} {size}: пустая маска",
                    icon.fluent_name()
                );
            }
        }
        assert!(
            raster(FluentIcon::Folder, 48).is_none(),
            "48 px не поставляется"
        );
    }

    #[test]
    fn tint_replaces_rgb_and_keeps_alpha() {
        let icon = FluentIcon::Folder;
        let size = 24;
        let source = raster(icon, size).expect("ассет");
        let tint = Color::rgb(200, 40, 10);
        let mut canvas = Canvas::filled(64, 64, Color::BLACK);
        draw(
            &mut canvas,
            icon,
            size,
            Rect::new(20, 20, size as i32, size as i32),
            tint,
            255,
        );
        for y in 0..size {
            for x in 0..size {
                let alpha = source.alpha(x, y);
                let expected = Color::rgba(200, 40, 10, alpha).blend_over(Color::BLACK);
                assert_eq!(
                    canvas.pixel(Point::new(20 + x as i32, 20 + y as i32)),
                    expected,
                    "пиксель {x},{y}"
                );
            }
        }
    }

    #[test]
    fn categories_have_distinct_rasters() {
        let a = raster(FluentIcon::Image, 32).unwrap();
        let b = raster(FluentIcon::Document, 32).unwrap();
        let c = raster(FluentIcon::MusicNote, 32).unwrap();
        assert_ne!(a.rgba, b.rgba);
        assert_ne!(b.rgba, c.rgba);
    }
}
