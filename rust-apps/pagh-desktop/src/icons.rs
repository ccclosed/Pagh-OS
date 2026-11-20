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
//! Размеры 16/20/24/32/48 поставляются как отдельные PNG: каждый отрендерен
//! cairosvg из нативного Fluent-размера в ТОЧНЫЙ размер, поэтому растр
//! никогда не масштабируется — апскейла нет. Любой другой размер (файла нет)
//! получает плавный пересэмпл ближайшего нативного — билинейно вверх,
//! box-площадью вниз ([`raster_at`], [`resample`]); nearest не используется
//! нигде: он и давал «рваные» штрихи и пиксельную лестницу.
//!
//! Глифы, у которых Fluent не имеет нативного целевого размера
//! (`Window Console` есть только в 20 px, `Grid` — до 28), отрисованы из
//! ближайшего нативного дизайна: это апскейл ВЕКТОРА, а не растра. Он
//! гладкий, но штрих выходит чуть тяжелее/мягче нативного дизайна — предел
//! самого набора, а не рендера.
//!
//! Лицензия и атрибуция: `assets/icons/LICENSE`, раздел «Иконки» в README.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::canvas::Canvas;
use crate::color::{self, Color};
use crate::geometry::{Point, Rect};

/// Доступные размеры ассетов.
pub const SIZES: [u32; 5] = [16, 20, 24, 32, 48];

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
            (Self::Grid, 48) => include_bytes!("../assets/icons/grid-48.png"),
            (Self::Terminal, 16) => include_bytes!("../assets/icons/terminal-16.png"),
            (Self::Terminal, 20) => include_bytes!("../assets/icons/terminal-20.png"),
            (Self::Terminal, 24) => include_bytes!("../assets/icons/terminal-24.png"),
            (Self::Terminal, 32) => include_bytes!("../assets/icons/terminal-32.png"),
            (Self::Terminal, 48) => include_bytes!("../assets/icons/terminal-48.png"),
            (Self::Folder, 16) => include_bytes!("../assets/icons/folder-16.png"),
            (Self::Folder, 20) => include_bytes!("../assets/icons/folder-20.png"),
            (Self::Folder, 24) => include_bytes!("../assets/icons/folder-24.png"),
            (Self::Folder, 32) => include_bytes!("../assets/icons/folder-32.png"),
            (Self::Folder, 48) => include_bytes!("../assets/icons/folder-48.png"),
            (Self::Image, 16) => include_bytes!("../assets/icons/image-16.png"),
            (Self::Image, 20) => include_bytes!("../assets/icons/image-20.png"),
            (Self::Image, 24) => include_bytes!("../assets/icons/image-24.png"),
            (Self::Image, 32) => include_bytes!("../assets/icons/image-32.png"),
            (Self::Image, 48) => include_bytes!("../assets/icons/image-48.png"),
            (Self::NetworkWired, 16) => include_bytes!("../assets/icons/network-wired-16.png"),
            (Self::NetworkWired, 20) => include_bytes!("../assets/icons/network-wired-20.png"),
            (Self::NetworkWired, 24) => include_bytes!("../assets/icons/network-wired-24.png"),
            (Self::NetworkWired, 32) => include_bytes!("../assets/icons/network-wired-32.png"),
            (Self::NetworkWired, 48) => include_bytes!("../assets/icons/network-wired-48.png"),
            (Self::WindowMinimize, 16) => include_bytes!("../assets/icons/window-minimize-16.png"),
            (Self::WindowMinimize, 20) => include_bytes!("../assets/icons/window-minimize-20.png"),
            (Self::WindowMinimize, 24) => include_bytes!("../assets/icons/window-minimize-24.png"),
            (Self::WindowMinimize, 32) => include_bytes!("../assets/icons/window-minimize-32.png"),
            (Self::WindowMinimize, 48) => include_bytes!("../assets/icons/window-minimize-48.png"),
            (Self::WindowMaximize, 16) => include_bytes!("../assets/icons/window-maximize-16.png"),
            (Self::WindowMaximize, 20) => include_bytes!("../assets/icons/window-maximize-20.png"),
            (Self::WindowMaximize, 24) => include_bytes!("../assets/icons/window-maximize-24.png"),
            (Self::WindowMaximize, 32) => include_bytes!("../assets/icons/window-maximize-32.png"),
            (Self::WindowMaximize, 48) => include_bytes!("../assets/icons/window-maximize-48.png"),
            (Self::WindowClose, 16) => include_bytes!("../assets/icons/window-close-16.png"),
            (Self::WindowClose, 20) => include_bytes!("../assets/icons/window-close-20.png"),
            (Self::WindowClose, 24) => include_bytes!("../assets/icons/window-close-24.png"),
            (Self::WindowClose, 32) => include_bytes!("../assets/icons/window-close-32.png"),
            (Self::WindowClose, 48) => include_bytes!("../assets/icons/window-close-48.png"),
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

/// Маска из одних альф (RGB остаётся чёрным — его заменит тинт).
fn raster_from_alpha(width: u32, height: u32, alpha: Vec<u8>) -> Raster {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for (index, value) in alpha.into_iter().enumerate() {
        rgba[index * 4 + 3] = value;
    }
    Raster {
        width,
        height,
        rgba,
    }
}

/// Пересэмплировать маску в `size`: билинейно при увеличении, box-площадью
/// при уменьшении.
///
/// Масштабирование идёт ТОЛЬКО по альфе: RGB у монохромного ассета везде
/// чёрный, а тинт ставит цвет после выборки. Поэтому нет ни премультипликации,
/// ни ореола цвета на краях. Nearest не используется нигде: при увеличении
/// соседние пиксели усредняются (край остаётся антиалиасенным, тонкий штрих
/// не превращается в «лестницу»), при уменьшении берётся средняя альфа по
/// покрытой площади (штрих не рвётся и не пропадает).
pub fn resample(source: &Raster, size: u32) -> Raster {
    let size = size.max(1);
    let (sw, sh) = (source.width.max(1), source.height.max(1));
    if sw == size && sh == size {
        return Raster {
            width: sw,
            height: sh,
            rgba: source.rgba.clone(),
        };
    }
    // Длина считается в usize: произведение сторон в u32 переполнилось бы
    // на абсурдном запросе и дало бы панику по индексу вместо пустой маски.
    let side = size as usize;
    let mut alpha = vec![0u8; side * side];
    let scale_x = sw as f64 / size as f64;
    let scale_y = sh as f64 / size as f64;
    let linear = size > sw || size > sh;
    for y in 0..size {
        for x in 0..size {
            let value = if linear {
                // Билинейная выборка по центрам исходных пикселей. Края
                // удерживаются в границах маски (clamp), чтобы не тянуть
                // пустоту за пределами глифа.
                let sx = ((x as f64 + 0.5) * scale_x - 0.5).max(0.0);
                let sy = ((y as f64 + 0.5) * scale_y - 0.5).max(0.0);
                let x0 = sx.floor() as u32;
                let y0 = sy.floor() as u32;
                let fx = sx - f64::from(x0);
                let fy = sy - f64::from(y0);
                let x1 = (x0 + 1).min(sw - 1);
                let y1 = (y0 + 1).min(sh - 1);
                let x0 = x0.min(sw - 1);
                let y0 = y0.min(sh - 1);
                let top = f64::from(source.alpha(x0, y0)) * (1.0 - fx)
                    + f64::from(source.alpha(x1, y0)) * fx;
                let bottom = f64::from(source.alpha(x0, y1)) * (1.0 - fx)
                    + f64::from(source.alpha(x1, y1)) * fx;
                top * (1.0 - fy) + bottom * fy
            } else {
                // Площадное среднее: окно dst-пикселя в координатах источника.
                let x0 = x as f64 * scale_x;
                let x1 = (x as f64 + 1.0) * scale_x;
                let y0 = y as f64 * scale_y;
                let y1 = (y as f64 + 1.0) * scale_y;
                let mut total = 0.0;
                let mut area = 0.0;
                for sy in y0.floor() as u32..y1.ceil() as u32 {
                    if sy >= sh {
                        break;
                    }
                    let overlap_y = (y1.min(f64::from(sy + 1)) - y0.max(f64::from(sy))).max(0.0);
                    if overlap_y == 0.0 {
                        continue;
                    }
                    for sx in x0.floor() as u32..x1.ceil() as u32 {
                        if sx >= sw {
                            break;
                        }
                        let overlap_x =
                            (x1.min(f64::from(sx + 1)) - x0.max(f64::from(sx))).max(0.0);
                        if overlap_x == 0.0 {
                            continue;
                        }
                        let weight = overlap_x * overlap_y;
                        total += f64::from(source.alpha(sx, sy)) * weight;
                        area += weight;
                    }
                }
                if area == 0.0 {
                    0.0
                } else {
                    total / area
                }
            };
            alpha[(y * size + x) as usize] = value.round().clamp(0.0, 255.0) as u8;
        }
    }
    raster_from_alpha(size, size, alpha)
}

/// Нативный размер ассета, ближайший к `size` (точное совпадение выигрывает).
///
/// Ближайший БОЛЬШИЙ размер предпочитается при равном расстоянии: уменьшение
/// площадным фильтром точнее увеличения билинейным.
fn source_size(icon: Icon, size: u32) -> Option<u32> {
    let mut best: Option<(u32, u32)> = None;
    for candidate in SIZES {
        if icon.png(candidate).is_none() {
            continue;
        }
        let distance = candidate.abs_diff(size);
        let better = match best {
            None => true,
            Some((best_size, best_distance)) => {
                distance < best_distance || (distance == best_distance && candidate > best_size)
            }
        };
        if better {
            best = Some((candidate, distance));
        }
    }
    best.map(|(candidate, _)| candidate)
}

type Cache = Mutex<HashMap<(Icon, u32), Arc<Raster>>>;

static CACHE: OnceLock<Cache> = OnceLock::new();

/// Декодированный ассет из кэша (декодируется при первом обращении).
///
/// Это НАТИВНЫЙ размер: файла `size` нет — `None`. Для отрисовки любого
/// размера — [`raster_at`].
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

/// Маска ровно `size`×`size` для отрисовки: нативный ассет, если он есть,
/// иначе плавный пересэмпл ближайшего нативного (см. [`resample`]).
///
/// Кэш общий с [`raster`]: нативный размер не копируется и не пересчитывается.
pub fn raster_at(icon: Icon, size: u32) -> Option<Arc<Raster>> {
    let size = size.max(1);
    if let Some(native) = raster(icon, size) {
        return Some(native);
    }
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = guard.get(&(icon, size)) {
            return Some(Arc::clone(cached));
        }
    }
    let source = raster(icon, source_size(icon, size)?)?;
    let scaled = Arc::new(resample(&source, size));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Гонка двух потоков допустима: оба получат эквивалентную маску,
    // в кэше останется первая.
    Some(Arc::clone(guard.entry((icon, size)).or_insert(scaled)))
}

/// Нарисовать иконку по центру `center`, перекрасив её в `tint`.
///
/// `size` — сторона квадрата в пикселях. Если для этого размера нет отдельного
/// PNG-ассета, маска плавно пересэмплируется из ближайшего нативного
/// ([`raster_at`]); нативный размер рисуется пиксель-в-пиксель, без потерь.
/// `opacity` масштабирует альфу (приглушение без смены цвета). Недоступный
/// ассет — тихий no-op: десктоп не должен падать из-за иконки.
pub fn draw(canvas: &mut Canvas, icon: Icon, size: u32, center: Point, tint: Color, opacity: u8) {
    let Some(raster) = raster_at(icon, size) else {
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
///
/// Блит идёт 1:1 по пикселям: маска обязана быть уже нужного размера (для
/// масштабирования — [`draw`]/[`raster_at`]). Альфа переносится как есть, без
/// квантования, поэтому полупрозрачная кайма штриха сохраняется: тонкий штрих
/// не обрывается и не становится ступенькой.
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
        // Полный контракт набора: 8 иконок x 16/20/24/32/48, без дыр.
        for icon in Icon::ALL {
            for size in SIZES {
                assert!(
                    icon.png(size).is_some(),
                    "{} {size}: ассет не поставляется",
                    icon.fluent_name()
                );
            }
        }
        assert!(raster(Icon::Grid, 40).is_none(), "40 px не поставляется");
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

    /// Все поставляемые ассеты должны иметь непрозрачное ядро: размытый
    /// глиф (max alpha < 190) читается как «рваные серые штрихи».
    /// Регрессия генерации (апскейл мелкого мастера) ловится здесь.
    #[test]
    fn every_asset_has_an_opaque_core() {
        for icon in Icon::ALL {
            for size in SIZES {
                let raster = raster(icon, size).expect("ассет");
                let peak = (0..size * size)
                    .map(|index| raster.rgba[index as usize * 4 + 3])
                    .max()
                    .unwrap();
                assert!(
                    peak >= 190,
                    "{} {size}: максимум альфы {peak} — глиф размыт",
                    icon.fluent_name()
                );
            }
        }
    }

    /// Штрихи глифов, которые десктоп реально рисует, непрозрачны: не меньше
    /// половины «туши» лежит в пикселях `alpha >= 200`.
    ///
    /// Это гейт на дефект «рваные/серые штрихи»: `Subtract` 16/20 в прежней
    /// сборке не имел ни одного пикселя выше 136 (полоса из двух строк по
    /// ~50 %), `Dismiss` 16 — выше 234. Фазовая подгонка при генерации
    /// ставит 1 px штрих на пиксельную сетку, и гейт не даёт это потерять.
    /// Размеры берутся из фактических вызовов: панель — 24 px (сеть 20),
    /// кнопки заголовка — 16.
    #[test]
    fn drawn_glyph_strokes_are_opaque() {
        let drawn = [
            (Icon::Grid, 24),
            (Icon::Terminal, 24),
            (Icon::Folder, 24),
            (Icon::Image, 24),
            (Icon::NetworkWired, 20),
            (Icon::NetworkWired, 24),
            (Icon::WindowMinimize, 16),
            (Icon::WindowMaximize, 16),
            (Icon::WindowClose, 16),
        ];
        for (icon, size) in drawn {
            let raster = raster(icon, size).expect("ассет");
            let (mut ink, mut solid) = (0u32, 0u32);
            for index in 0..(size * size) as usize {
                let alpha = u32::from(raster.rgba[index * 4 + 3]);
                ink += alpha;
                if alpha >= 200 {
                    solid += alpha;
                }
            }
            let share = f64::from(solid) / f64::from(ink.max(1));
            assert!(
                share >= 0.5,
                "{} {size}: только {share:.3} туши непрозрачно — штрих серый",
                icon.fluent_name()
            );
        }
    }

    /// Края панельных глифов антиалиасенные: у 24 px половина зажжённых
    /// пикселей обязана быть полупрозрачной каймой. Nearest-апскейл дал бы
    /// ровно 0/255 и провалил бы проверку.
    #[test]
    fn panel_glyphs_keep_antialiased_edges() {
        for icon in [
            Icon::Grid,
            Icon::Terminal,
            Icon::Folder,
            Icon::Image,
            Icon::NetworkWired,
        ] {
            for size in [20, 24] {
                let raster = raster(icon, size).expect("ассет");
                let (mut lit, mut soft) = (0, 0);
                for index in 0..(size * size) as usize {
                    let alpha = raster.rgba[index * 4 + 3];
                    if alpha > 2 {
                        lit += 1;
                        if alpha < 200 {
                            soft += 1;
                        }
                    }
                }
                assert!(lit > 8, "{} {size}: пустая маска", icon.fluent_name());
                assert!(
                    soft * 5 >= lit,
                    "{} {size}: каймы {soft} из {lit} — край без антиалиасинга",
                    icon.fluent_name()
                );
            }
        }
    }

    /// Нативный размер рисуется пиксель-в-пиксель: никакого пересэмпла.
    #[test]
    fn native_size_is_drawn_one_to_one() {
        for size in SIZES {
            let native = raster(Icon::Folder, size).expect("ассет");
            let drawn = raster_at(Icon::Folder, size).expect("маска");
            assert!(Arc::ptr_eq(&native, &drawn), "{size}: маска пересобрана");
        }
    }

    /// Размера без ассета больше не «нет»: он плавно пересэмплируется.
    #[test]
    fn unsupported_size_is_resampled_smoothly() {
        let raster = raster_at(Icon::Folder, 28).expect("пересэмпл");
        assert_eq!((raster.width, raster.height), (28, 28));
        let lit = (0..28 * 28)
            .filter(|index| raster.rgba[*index as usize * 4 + 3] > 0)
            .count();
        assert!(lit > 100, "28 px: слишком мало пикселей ({lit})");
        // Nearest выдал бы только альфы исходной маски (0/63/…/255 из 32 px);
        // билинейная выборка даёт промежуточные уровни, которых в источнике нет.
        let source = raster_at(Icon::Folder, 32).expect("нативный 32");
        let source_levels: std::collections::HashSet<u8> = (0..32 * 32)
            .map(|index| source.rgba[index * 4 + 3])
            .collect();
        let fresh = (0..28 * 28)
            .map(|index| raster.rgba[index * 4 + 3])
            .filter(|alpha| *alpha > 0 && !source_levels.contains(alpha))
            .count();
        assert!(
            fresh > 10,
            "28 px: нет ни одного нового уровня альфы ({fresh})"
        );
    }

    /// Увеличение билинейно: соседние пиксели усредняются, дыр не появляется.
    #[test]
    fn upscale_interpolates_neighbours() {
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                rgba[(y * 4 + x) * 4 + 3] = if x < 2 { 0 } else { 255 };
            }
        }
        let source = Raster {
            width: 4,
            height: 4,
            rgba,
        };
        let scaled = resample(&source, 8);
        assert_eq!((scaled.width, scaled.height), (8, 8));
        // Резкая граница x=2 размазывается на 2-3 пикселя: появляются
        // промежуточные альфы, а не дубли столбцов (как у nearest).
        let row: Vec<u8> = (0..8).map(|x| scaled.alpha(x, 4)).collect();
        assert!(
            row.iter().any(|alpha| *alpha > 20 && *alpha < 235),
            "ряд {row:?}"
        );
        assert!(row.windows(2).all(|pair| pair[0] <= pair[1]), "ряд {row:?}");
    }

    /// Уменьшение — площадное среднее: заливка остаётся заливкой, полукадр
    /// остаётся полукадром, а тонкий штрих не исчезает.
    #[test]
    fn downscale_averages_area() {
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                rgba[(y * 4 + x) * 4 + 3] = if x < 2 { 255 } else { 0 };
            }
        }
        let source = Raster {
            width: 4,
            height: 4,
            rgba,
        };
        let scaled = resample(&source, 2);
        // Левый столбец целиком из 255, правый — из 0: средние 255 и 0.
        assert_eq!(scaled.alpha(0, 0), 255);
        assert_eq!(scaled.alpha(1, 0), 0);
        // Один тонкий ряд (1 px из 4) на 1/4 площади даёт ~64, а не 0.
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for x in 0..4 {
            rgba[x * 4 + 3] = 255;
        }
        let thin = resample(
            &Raster {
                width: 4,
                height: 4,
                rgba,
            },
            2,
        );
        assert_eq!(
            thin.alpha(0, 0),
            128,
            "тонкий ряд должен усредниться, а не пропасть"
        );
    }

    /// Отрисовка нестандартного размера попадает ровно в запрошенный квадрат.
    #[test]
    fn draw_fills_requested_size() {
        let mut canvas = Canvas::new(80, 80, rgb(0, 0, 0));
        draw(
            &mut canvas,
            Icon::Terminal,
            28,
            Point::new(40, 40),
            rgb(255, 255, 255),
            255,
        );
        let origin = 40 - 28 / 2;
        let (mut inside, mut outside) = (0, 0);
        for y in 0..80 {
            for x in 0..80 {
                let lit = crate::color::red(canvas.get(x, y)) > 0;
                let in_box = x >= origin && x < origin + 28 && y >= origin && y < origin + 28;
                if lit && in_box {
                    inside += 1;
                }
                if lit && !in_box {
                    outside += 1;
                }
            }
        }
        assert!(inside > 100, "иконка не нарисована ({inside})");
        assert_eq!(outside, 0, "пиксели за пределами 28×28");
    }
}
