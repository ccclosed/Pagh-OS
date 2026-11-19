//! Нижняя icon-only панель (44 px): часы/дата, сеть, лаунчер и чипы окон.

use crate::canvas::Canvas;
use crate::geometry::{Point, Rect};
use crate::icons::{self, Icon};
use crate::netinfo::NetInfo;
use crate::theme::Theme;

/// Высота панели в пикселях.
pub const PANEL_HEIGHT: i32 = 44;
const PADDING: i32 = 8;
const ICON_CELL: i32 = 44;
const CLOCK_WIDTH: i32 = 104;
const NETWORK_WIDTH: i32 = 36;
const CLOCK_FONT_SCALE: i32 = 1;

/// Иконки приложений на панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelIcon {
    Launcher,
    Terminal,
    Files,
    Paint,
}

impl PanelIcon {
    pub const ALL: [PanelIcon; 4] = [
        PanelIcon::Launcher,
        PanelIcon::Terminal,
        PanelIcon::Files,
        PanelIcon::Paint,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Launcher => 0,
            Self::Terminal => 1,
            Self::Files => 2,
            Self::Paint => 3,
        }
    }
}

/// Что находится под указателем в панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHit {
    Icon(PanelIcon),
    WindowChip(u32),
    Network,
    Clock,
    Background,
}

/// Подсветка на панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHover {
    Icon(usize),
    WindowChip(u32),
    Network,
    Clock,
}

pub fn panel_rect(screen: Rect) -> Rect {
    Rect::new(
        screen.x,
        screen.bottom() - PANEL_HEIGHT,
        screen.w,
        PANEL_HEIGHT,
    )
}

pub fn icon_rect(screen: Rect, index: usize) -> Rect {
    let panel = panel_rect(screen);
    Rect::new(
        panel.x + PADDING + index as i32 * ICON_CELL,
        panel.y + (PANEL_HEIGHT - ICON_CELL) / 2,
        ICON_CELL,
        ICON_CELL,
    )
}

pub fn clock_rect(screen: Rect) -> Rect {
    let panel = panel_rect(screen);
    Rect::new(
        panel.right() - PADDING - CLOCK_WIDTH,
        panel.y + 2,
        CLOCK_WIDTH,
        PANEL_HEIGHT - 4,
    )
}

pub fn network_rect(screen: Rect) -> Rect {
    let clock = clock_rect(screen);
    Rect::new(
        clock.x - 10 - NETWORK_WIDTH,
        panel_rect(screen).y + (PANEL_HEIGHT - NETWORK_WIDTH) / 2,
        NETWORK_WIDTH,
        NETWORK_WIDTH,
    )
}

/// Область, отведённая под чипы свёрнутых/открытых окон.
pub fn chip_area(screen: Rect) -> Rect {
    let panel = panel_rect(screen);
    let first = icon_rect(screen, PanelIcon::ALL.len()).x + PADDING;
    let last = network_rect(screen).x - 12;
    Rect::new(first, panel.y + 7, (last - first).max(0), PANEL_HEIGHT - 14)
}

pub const CHIP_WIDTH: i32 = 30;
const CHIP_GAP: i32 = 4;

pub fn chip_rect(screen: Rect, slot: usize) -> Rect {
    let area = chip_area(screen);
    Rect::new(
        area.x + slot as i32 * (CHIP_WIDTH + CHIP_GAP),
        area.y,
        CHIP_WIDTH,
        area.h,
    )
}

/// Сеть на панели: три честных состояния вместо прежнего `bool`.
///
/// `NoData` — сервер ещё не прочитал `/proc/net/dev` (или канала нет); это не
/// «офлайн», поэтому иконка приглушена, но диагонали «нет сети» нет: неизвестность
/// нельзя выдавать за отключение.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetState {
    NoData,
    Offline,
    Online,
}

/// Состояние панели: часы, дата и реальные сетевые данные.
#[derive(Clone, Debug)]
pub struct Panel {
    pub time: String,
    pub date: String,
    /// Сетевой сэмпл из ядра (`/proc/net/dev` + `SIOCGIF*`). `None` — данных
    /// ещё нет: панель не выдумывает «онлайн» по умолчанию.
    pub network: Option<NetInfo>,
    pub hover: Option<PanelHover>,
}

impl Panel {
    pub fn new(time: impl Into<String>, date: impl Into<String>) -> Self {
        Self {
            time: time.into(),
            date: date.into(),
            network: None,
            hover: None,
        }
    }

    /// Состояние иконки: онлайн только при реальном `IFF_UP | IFF_RUNNING`.
    pub fn net_state(&self) -> NetState {
        match &self.network {
            None => NetState::NoData,
            Some(info) if info.up && info.running => NetState::Online,
            Some(_) => NetState::Offline,
        }
    }

    /// Статическая геометрия без динамических чипов окон.
    pub fn hit_static(&self, screen: Rect, point: Point) -> Option<PanelHit> {
        if !panel_rect(screen).contains(point) {
            return None;
        }
        for (index, icon) in PanelIcon::ALL.iter().enumerate() {
            if icon_rect(screen, index).contains(point) {
                return Some(PanelHit::Icon(*icon));
            }
        }
        if network_rect(screen).contains(point) {
            return Some(PanelHit::Network);
        }
        if clock_rect(screen).contains(point) {
            return Some(PanelHit::Clock);
        }
        Some(PanelHit::Background)
    }

    /// Чип окна под указателем; `chips` — (window_id, подпись) в порядке отрисовки.
    pub fn hit_chip(
        &self,
        screen: Rect,
        point: Point,
        chips: &[(u32, String, bool)],
    ) -> Option<u32> {
        if !chip_area(screen).contains(point) {
            return None;
        }
        for (slot, (id, _, _)) in chips.iter().enumerate() {
            if chip_rect(screen, slot).contains(point) {
                return Some(*id);
            }
        }
        None
    }

    /// Нарисовать панель. `chips` — (window_id, подпись, окно сфокусировано).
    pub fn draw(
        &self,
        canvas: &mut Canvas,
        screen: Rect,
        theme: &Theme,
        hover: Option<PanelHover>,
        chips: &[(u32, String, bool)],
    ) {
        let panel = panel_rect(screen);
        canvas.fill_gradient(panel, theme.panel_bg_alt, theme.panel_bg);
        canvas.fill_rect(Rect::new(panel.x, panel.y, panel.w, 1), theme.panel_border);

        for (index, icon) in PanelIcon::ALL.iter().enumerate() {
            let rect = icon_rect(screen, index);
            if hover == Some(PanelHover::Icon(index)) {
                canvas.fill_rounded_rect(rect.inset(4), 8, theme.panel_hover, 220);
            }
            draw_icon(canvas, *icon, rect, theme);
        }

        for (slot, (id, label, focused)) in chips.iter().enumerate() {
            let rect = chip_rect(screen, slot);
            if rect.right() > network_rect(screen).x - 6 {
                break;
            }
            let hovered = hover == Some(PanelHover::WindowChip(*id));
            let bg = if hovered {
                theme.panel_hover
            } else {
                theme.chip_bg
            };
            canvas.fill_rounded_rect(rect, 7, bg, 240);
            let border = if *focused {
                theme.accent
            } else {
                theme.panel_border
            };
            canvas.stroke_rounded_rect(rect, 7, border, 255);
            let ch = label.chars().next().unwrap_or('?').to_ascii_uppercase();
            let mut text = [0u8; 4];
            let s = ch.encode_utf8(&mut text);
            let x = rect.x + (rect.w - 8) / 2;
            canvas.text(x, rect.y + (rect.h - 16) / 2, 1, theme.chip_text, s);
        }

        // Сеть: проводной порт (ethernet). Нет линка — приглушён и перечёркнут,
        // нет данных — только приглушён (неизвестность не равна отключению).
        let net = network_rect(screen);
        if hover == Some(PanelHover::Network) {
            canvas.fill_rounded_rect(net.inset(2), 8, theme.panel_hover, 220);
        }
        draw_network(canvas, net, theme, self.net_state());

        // Часы и дата, выровненные по правому краю блока.
        let clock = clock_rect(screen);
        if hover == Some(PanelHover::Clock) {
            canvas.fill_rounded_rect(clock.inset(2), 8, theme.panel_hover, 160);
        }
        let time_w = self.time.chars().count() as i32 * 8 * CLOCK_FONT_SCALE;
        let date_w = self.date.chars().count() as i32 * 8 * CLOCK_FONT_SCALE;
        canvas.text(
            clock.right() - 4 - time_w,
            clock.y + 3,
            CLOCK_FONT_SCALE,
            theme.panel_text,
            &self.time,
        );
        canvas.text(
            clock.right() - 4 - date_w,
            clock.y + 22,
            CLOCK_FONT_SCALE,
            theme.panel_text_dim,
            &self.date,
        );
    }
}

fn draw_icon(canvas: &mut Canvas, icon: PanelIcon, cell: Rect, theme: &Theme) {
    // Готовые regular-глифы Fluent (assets/icons), 24 px в ячейке 44 px.
    // Иконки монохромные: тинт задаёт цвет, поэтому лаунчер/терминал/файлы
    // берут обычный цвет панели, а Paint (единственный «цветной» апп в
    // референсе) — акцент.
    let (asset, tint) = match icon {
        PanelIcon::Launcher => (Icon::Grid, theme.panel_icon),
        PanelIcon::Terminal => (Icon::Terminal, theme.panel_icon),
        PanelIcon::Files => (Icon::Folder, theme.panel_icon),
        PanelIcon::Paint => (Icon::Image, theme.accent),
    };
    icons::draw(canvas, asset, 24, cell.center(), tint, 255);
}

/// Сеть: готовый глиф Plug Connected (проводная сеть), а не Wi-Fi-дуги.
///
/// У машины нет Wi-Fi: линк приходит либо с e1000 (провод), либо с RNDIS
/// поверх USB (тетеринг телефона), а референс Win11 «Сети» — «Проводное
/// подключение». Та же иконка, что в карточке подключения во флайауте
/// (`compositor::draw_wired_icon`), поэтому панель и флайаут показывают
/// один и тот же тип линка.
///
/// Офлайн: глиф приглушается в `panel_icon_dim`, поверх сохраняется
/// диагональ «нет сети» цветом `button_close`. Нет данных (`NoData`) — тот же
/// приглушённый глиф без диагонали: штрих утверждал бы «сети нет», а мы этого
/// не знаем.
fn draw_network(canvas: &mut Canvas, rect: Rect, theme: &Theme, state: NetState) {
    let online = state == NetState::Online;
    let color = if online {
        theme.panel_icon
    } else {
        theme.panel_icon_dim
    };
    let center = rect.center();
    icons::draw(canvas, Icon::NetworkWired, 20, center, color, 255);
    if state == NetState::Offline {
        canvas.line(
            Point::new(center.x - 11, center.y + 11),
            Point::new(center.x + 11, center.y - 11),
            theme.button_close,
            2,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0, 0, 1280, 720);

    #[test]
    fn panel_is_44px_at_bottom() {
        let panel = panel_rect(SCREEN);
        assert_eq!(panel.y, 720 - 44);
        assert_eq!(panel.h, 44);
        assert_eq!(panel.w, 1280);
    }

    #[test]
    fn controls_do_not_overlap() {
        let clock = clock_rect(SCREEN);
        let network = network_rect(SCREEN);
        let area = chip_area(SCREEN);
        assert!(clock.contains(clock.center()));
        assert!(network.right() < clock.x);
        assert!(area.right() < network.x);
        assert!(area.x > icon_rect(SCREEN, 3).right());
        for i in 0..4 {
            assert!(!icon_rect(SCREEN, i).intersects(icon_rect(SCREEN, i + 1)));
        }
    }

    #[test]
    fn static_hits_resolve_controls() {
        let panel = Panel::new("23:38", "15.11.2026");
        assert_eq!(
            panel.hit_static(SCREEN, icon_rect(SCREEN, 1).center()),
            Some(PanelHit::Icon(PanelIcon::Terminal))
        );
        assert_eq!(
            panel.hit_static(SCREEN, network_rect(SCREEN).center()),
            Some(PanelHit::Network)
        );
        assert_eq!(
            panel.hit_static(SCREEN, clock_rect(SCREEN).center()),
            Some(PanelHit::Clock)
        );
        assert_eq!(
            panel.hit_static(SCREEN, Point::new(500, 700)),
            Some(PanelHit::Background)
        );
        assert_eq!(panel.hit_static(SCREEN, Point::new(500, 600)), None);
    }

    #[test]
    fn draws_clock_and_network_pixels() {
        let mut canvas = Canvas::new(1280, 720, crate::color::rgb(0, 0, 0));
        let panel = Panel::new("23:38", "15.11.2026");
        panel.draw(&mut canvas, SCREEN, &Theme::default(), None, &[]);
        // Часы и дата оставляют пиксели в своём блоке.
        let clock = clock_rect(SCREEN);
        let lit = canvas
            .pixels()
            .chunks(1280)
            .skip(clock.y as usize)
            .take(clock.h as usize)
            .flat_map(|row| &row[clock.x as usize..clock.right() as usize])
            .filter(|p| crate::color::red(**p) > 180)
            .count();
        assert!(
            lit > 40,
            "часы и дата должны быть нарисованы, пикселей {lit}"
        );
    }

    /// Сэмпл сети для пиксельных проверок: поля реальные, значения — эталонные.
    fn test_net(running: bool) -> NetInfo {
        NetInfo {
            iface: crate::netinfo::IFACE.to_string(),
            ip: Some(std::net::Ipv4Addr::new(10, 0, 2, 15)),
            prefix: Some(24),
            mac: Some([0x52, 0x54, 0, 12, 34, 56]),
            up: true,
            running,
            rx_bytes: 1234,
            tx_bytes: 567,
            rx_packets: 10,
            tx_packets: 7,
            rx_rate: 1200,
            tx_rate: 340,
            at: std::time::Instant::now(),
        }
    }

    /// Собрать кадр панели для пиксельных проверок иконки сети.
    fn panel_frame(online: bool) -> Canvas {
        let mut canvas = Canvas::new(1280, 720, crate::color::rgb(0, 0, 0));
        let mut panel = Panel::new("23:38", "15.11.2026");
        panel.network = Some(test_net(online));
        panel.draw(&mut canvas, SCREEN, &Theme::default(), None, &[]);
        canvas
    }

    /// Пиксели блока сети, отличающиеся от фона своей строки сильнее порога.
    ///
    /// Фон панели — вертикальный градиент, поэтому сравнение идёт с цветом
    /// той же строки вне иконки (x = 500 — пустая область панели). Порог
    /// отсекает антиалиасинг и позволяет отдельно мерить «яркую» (обычный
    /// цвет) и «приглушённую» (dim) иконку.
    fn lit_pixels(canvas: &Canvas, threshold: i32) -> Vec<(i32, i32)> {
        let net = network_rect(SCREEN);
        let mut lit = Vec::new();
        for y in net.y..net.bottom() {
            let bg = canvas.get(500, y);
            for x in net.x..net.right() {
                let pixel = canvas.get(x, y);
                let delta = (i32::from(crate::color::red(pixel))
                    - i32::from(crate::color::red(bg)))
                .abs()
                    + (i32::from(crate::color::green(pixel)) - i32::from(crate::color::green(bg)))
                        .abs()
                    + (i32::from(crate::color::blue(pixel)) - i32::from(crate::color::blue(bg)))
                        .abs();
                if delta > threshold {
                    lit.push((x, y));
                }
            }
        }
        lit
    }

    #[test]
    fn network_icon_is_a_wired_plug_not_wifi_arcs() {
        let canvas = panel_frame(true);
        let center = network_rect(SCREEN).center();
        let lit = lit_pixels(&canvas, 40);
        assert!(
            lit.len() > 40,
            "иконка сети не нарисована, пикселей {}",
            lit.len()
        );

        // Готовый глиф Plug Connected 20×20: альфа занимает 2..=17, то есть
        // ±8 от центра. Wi-Fi-дуги старого образца поднимались на ~14 px выше
        // центра и расходились до ±15 — такие пиксели здесь были бы багом.
        let (mut min_x, mut max_x) = (i32::MAX, i32::MIN);
        let (mut min_y, mut max_y) = (i32::MAX, i32::MIN);
        for &(x, y) in &lit {
            min_x = min_x.min(x - center.x);
            max_x = max_x.max(x - center.x);
            min_y = min_y.min(y - center.y);
            max_y = max_y.max(y - center.y);
        }
        assert!(
            min_x >= -11 && max_x <= 11,
            "иконка шире проводной формы: x [{min_x}, {max_x}] (дуги давали ±15)"
        );
        assert!(
            min_y >= -10 && max_y <= 10,
            "иконка выше проводной формы: y [{min_y}, {max_y}] (дуги давали -14)"
        );

        // Глиф реально закрашен, а не пустая рамка: вокруг центра есть пиксели.
        let near_center = lit
            .iter()
            .filter(|(x, y)| (x - center.x).abs() <= 4 && (y - center.y).abs() <= 4)
            .count();
        assert!(
            near_center >= 2,
            "у центра иконки сети нет закрашенных пикселей: {near_center}"
        );
    }

    /// Пиксель глифа с максимальной альфой вне диагонали «нет сети».
    ///
    /// Диагональ идёт через центр, поэтому для проверки тинта берётся
    /// непрозрачный пиксель подальше от неё (иначе `button_close` перекрывает
    /// цвет иконки).
    fn opaque_off_diagonal() -> (i32, i32, u8) {
        let raster = crate::icons::raster(Icon::NetworkWired, 20).expect("ассет иконки");
        let mut best = (0i32, 0i32, 0u8);
        for y in 0..raster.height as i32 {
            for x in 0..raster.width as i32 {
                let alpha = raster.alpha(x as u32, y as u32);
                let diagonal = (x + y - raster.width as i32).abs();
                if alpha > best.2 && diagonal >= 4 {
                    best = (x, y, alpha);
                }
            }
        }
        assert!(best.2 > 200, "не нашлось непрозрачного пикселя глифа");
        best
    }

    #[test]
    fn network_icon_dims_and_strikes_when_offline() {
        let theme = Theme::default();
        let online = panel_frame(true);
        let offline = panel_frame(false);
        let center = network_rect(SCREEN).center();

        // Один и тот же глиф: в онлайне — обычный цвет, в офлайне — dim.
        // Пиксель с максимальной альфой даёт предсказуемый результат blend.
        let (source_x, source_y, alpha) = opaque_off_diagonal();
        let point = Point::new(center.x - 10 + source_x, center.y - 10 + source_y);
        let expected = |frame: &Canvas, tint: crate::color::Color| {
            let background = frame.get(500, point.y);
            crate::color::blend(
                background,
                crate::color::rgba(
                    crate::color::red(tint),
                    crate::color::green(tint),
                    crate::color::blue(tint),
                    alpha,
                ),
                255,
            )
        };
        assert_eq!(
            online.get(point.x, point.y),
            expected(&online, theme.panel_icon),
            "онлайн-иконка должна быть цветом panel_icon"
        );
        assert_eq!(
            offline.get(point.x, point.y),
            expected(&offline, theme.panel_icon_dim),
            "офлайн-иконка должна быть цветом panel_icon_dim"
        );
        assert!(
            lit_pixels(&offline, 20).len() > 40,
            "в офлайне иконка должна оставаться видимой"
        );

        // Диагональ «нет сети» — единственное, что красится `button_close`.
        let struck = offline
            .pixels()
            .iter()
            .filter(|p| **p == theme.button_close)
            .count();
        assert!(struck > 0, "в отключённом состоянии нет диагонали");
        // ...и её нет в онлайне.
        let struck_online = online
            .pixels()
            .iter()
            .filter(|p| **p == theme.button_close)
            .count();
        assert_eq!(struck_online, 0, "в онлайне диагонали быть не должно");

        // Центр блока — прозрачная середина глифа, не заливка.
        assert_ne!(
            online.get(center.x, center.y),
            theme.panel_icon,
            "центр иконки не должен быть сплошной заливкой"
        );
    }

    /// Нет данных (`None`) — приглушённый глиф без диагонали: «неизвестно» не
    /// притворяется ни «онлайн», ни «отключено».
    #[test]
    fn network_icon_without_data_is_dim_and_unstruck() {
        let theme = Theme::default();
        let mut canvas = Canvas::new(1280, 720, crate::color::rgb(0, 0, 0));
        let panel = Panel::new("23:38", "15.11.2026");
        assert_eq!(panel.net_state(), NetState::NoData);
        panel.draw(&mut canvas, SCREEN, &theme, None, &[]);

        let center = network_rect(SCREEN).center();
        let (source_x, source_y, alpha) = opaque_off_diagonal();
        let point = Point::new(center.x - 10 + source_x, center.y - 10 + source_y);
        let background = canvas.get(500, point.y);
        let expected = crate::color::blend(
            background,
            crate::color::rgba(
                crate::color::red(theme.panel_icon_dim),
                crate::color::green(theme.panel_icon_dim),
                crate::color::blue(theme.panel_icon_dim),
                alpha,
            ),
            255,
        );
        assert_eq!(
            canvas.get(point.x, point.y),
            expected,
            "без данных иконка должна быть panel_icon_dim"
        );
        assert!(
            lit_pixels(&canvas, 20).len() > 40,
            "без данных иконка должна оставаться видимой"
        );
        let struck = canvas
            .pixels()
            .iter()
            .filter(|p| **p == theme.button_close)
            .count();
        assert_eq!(struck, 0, "без данных диагонали «нет сети» быть не должно");
    }
}
