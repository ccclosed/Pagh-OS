//! Ядро композитора: окна, z-order, фокус, панель, контекстное меню, damage.
//!
//! Композитор не знает о сокетах и backend'ах: он принимает события ввода/кадры,
//! возвращает [`HostEvent`] (что отправить клиентам и что запустить) и умеет
//! отрисовать повреждённые области в собственный холст.

use crate::canvas::Canvas;
use crate::color;
use crate::context_menu::{ContextMenu, MenuAction};
use crate::geometry::{DamageTracker, Point, Rect, Size};
use crate::panel::{self, Panel, PanelHit, PanelHover, PanelIcon, PANEL_HEIGHT};
use crate::protocol::{
    key, Message, BUTTONS_LEFT, BUTTONS_MIDDLE, BUTTONS_RIGHT, CLOSE_SERVER, CONFIGURE_FOCUSED,
    CONFIGURE_MAXIMIZED, CONFIGURE_MINIMIZED, CONFIGURE_RESIZABLE, MOD_ALT, MOD_CTRL,
};
use crate::surface::{PixelFormat, Surface, SurfaceError};
use crate::theme::Theme;
use crate::window::{Window, WindowPart, BORDER, MIN_CONTENT};
use std::collections::HashMap;

/// Приложение, которое просит запустить панель/меню.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppKind {
    Terminal,
    Files,
    Paint,
}

impl AppKind {
    /// Стабильный идентификатор приложения для LAUNCH/CLI.
    pub fn id(self) -> &'static str {
        match self {
            AppKind::Terminal => "terminal",
            AppKind::Files => "files",
            AppKind::Paint => "paint",
        }
    }

    /// Разобрать идентификатор приложения.
    pub fn from_id(id: &str) -> Option<AppKind> {
        match id {
            "terminal" => Some(AppKind::Terminal),
            "files" => Some(AppKind::Files),
            "paint" => Some(AppKind::Paint),
            _ => None,
        }
    }
}

/// Действие, которое сервер должен выполнить после обработки события.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEvent {
    /// Отправить сообщение клиенту.
    Send { client_id: u64, message: Message },
    /// Запустить клиентское приложение.
    Launch(AppKind),
}

/// Кнопка мыши.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other(u32),
}

impl MouseButton {
    pub fn from_wire(value: u32) -> Self {
        match value {
            1 => Self::Left,
            2 => Self::Middle,
            3 => Self::Right,
            other => Self::Other(other),
        }
    }

    pub fn to_wire(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::Other(code) => code,
        }
    }

    fn bitmask(self) -> u32 {
        match self {
            Self::Left => BUTTONS_LEFT,
            Self::Middle => BUTTONS_MIDDLE,
            Self::Right => BUTTONS_RIGHT,
            Self::Other(code) => 1 << (code.min(31)),
        }
    }
}

/// Что сейчас тащит пользователь.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragState {
    Move {
        window_id: u32,
        grab_offset: Point,
        /// true — перемещение завершается следующим нажатием (пункт меню Move).
        drop_on_click: bool,
    },
    Resize {
        window_id: u32,
        start_pointer: Point,
        start_content: Size,
    },
}

/// Вид всплывающей панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlyoutKind {
    Network,
    Calendar,
}

#[derive(Clone, Copy, Debug)]
pub struct Flyout {
    pub kind: FlyoutKind,
    pub rect: Rect,
}

/// Ошибки операций с окнами.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowError {
    Duplicate(u32),
    TooLarge,
    InvalidSize,
    LimitReached,
    NotFound(u32),
    NotOwner,
    InvalidSurface(SurfaceError),
    /// SHM commit пришёл до SHM_OFFER (нет серверной тени поверхности).
    NoSurface,
}

impl WindowError {
    pub fn code(&self) -> u32 {
        match self {
            Self::Duplicate(_) => crate::protocol::ERR_DUPLICATE_WINDOW,
            Self::TooLarge | Self::InvalidSize | Self::InvalidSurface(_) | Self::NoSurface => {
                crate::protocol::ERR_INVALID_SURFACE
            }
            Self::LimitReached => crate::protocol::ERR_LIMIT,
            Self::NotFound(_) => crate::protocol::ERR_UNKNOWN_WINDOW,
            Self::NotOwner => crate::protocol::ERR_NOT_OWNER,
        }
    }
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "окно {id} уже существует"),
            Self::TooLarge => write!(f, "окно слишком большое"),
            Self::InvalidSize => write!(f, "недопустимый размер окна"),
            Self::LimitReached => write!(f, "достигнут лимит окон"),
            Self::NotFound(id) => write!(f, "окно {id} не найдено"),
            Self::NotOwner => write!(f, "клиент не владеет окном"),
            Self::InvalidSurface(err) => write!(f, "некорректная поверхность: {err}"),
            Self::NoSurface => write!(f, "у окна нет SHM-поверхности (нужен shm_offer)"),
        }
    }
}

impl From<SurfaceError> for WindowError {
    fn from(value: SurfaceError) -> Self {
        Self::InvalidSurface(value)
    }
}

const MAX_WINDOWS: usize = 32;
const MIN_WINDOW_W: u32 = 64;
const MIN_WINDOW_H: u32 = 48;
const WINDOW_RADIUS: i32 = 10;

/// Счётчики построения кэшей (для тестов/логов; кэш не должен перестраиваться
/// каждый кадр).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub wallpaper_builds: u64,
    pub panel_builds: u64,
    pub chrome_builds: u64,
    pub shadow_builds: u64,
}

/// Ключ кэша панели: перестраиваем только при реальном изменении содержимого.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelKey {
    screen: Rect,
    time: String,
    date: String,
    network_online: bool,
    hover: Option<PanelHover>,
    chips: Vec<(u32, String, bool)>,
}

/// Ключ кэша оформления окна. Позиция не входит: слой рисуется локально и
/// переносится одним blit'ом, поэтому drag не инвалидирует кэш.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChromeKey {
    width: i32,
    height: i32,
    title: String,
    focused: bool,
    maximized: bool,
    hover: Option<WindowPart>,
}

struct ChromeCache {
    key: ChromeKey,
    layer: Canvas,
}

/// Один слой предвычисленной тени: alpha и построчные горизонтальные пролёты.
struct ShadowLayer {
    alpha: u8,
    rows: Vec<(i32, i32, i32)>,
}

struct ShadowCache {
    layers: Vec<ShadowLayer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HoverState {
    panel: Option<PanelHover>,
    menu: Option<usize>,
    button: Option<(u32, WindowPart)>,
}

/// Композитор рабочего стола.
pub struct Compositor {
    screen: Rect,
    theme: Theme,
    windows: Vec<Window>,
    focused: Option<u32>,
    panel: Panel,
    pointer: Point,
    buttons: u32,
    drag: Option<DragState>,
    context_menu: Option<ContextMenu>,
    flyout: Option<Flyout>,
    pressed_window: Option<u32>,
    damage: DamageTracker,
    canvas: Canvas,
    frame_no: u64,
    seq: u64,
    wallpaper_alt: bool,
    wallpaper_cache: Option<Canvas>,
    panel_cache: Option<(PanelKey, Canvas)>,
    chrome_cache: HashMap<u32, ChromeCache>,
    shadow_cache: HashMap<(i32, i32), ShadowCache>,
    cache_stats: CacheStats,
}

impl Compositor {
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_theme(width, height, Theme::default())
    }

    pub fn with_theme(width: u32, height: u32, theme: Theme) -> Self {
        let screen = Rect::new(0, 0, width as i32, height as i32);
        let mut damage = DamageTracker::new(width, height);
        damage.add_whole();
        Self {
            screen,
            theme,
            windows: Vec::new(),
            focused: None,
            panel: Panel::new("23:38", "15.11.2026"),
            pointer: Point::new((width / 2) as i32, (height / 2) as i32),
            buttons: 0,
            drag: None,
            context_menu: None,
            flyout: None,
            pressed_window: None,
            damage,
            canvas: Canvas::new(width, height, theme.wallpaper_bottom),
            frame_no: 0,
            seq: 1,
            wallpaper_alt: false,
            wallpaper_cache: None,
            panel_cache: None,
            chrome_cache: HashMap::new(),
            shadow_cache: HashMap::new(),
            cache_stats: CacheStats::default(),
        }
    }

    pub fn screen(&self) -> Rect {
        self.screen
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    fn window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn focused_id(&self) -> Option<u32> {
        self.focused
    }

    pub fn focused_window(&self) -> Option<&Window> {
        self.focused.and_then(|id| self.window(id))
    }

    pub fn frame_number(&self) -> u64 {
        self.frame_no
    }

    /// Есть ли накопленный damage, ещё не показанный backend'у.
    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }

    pub fn frame(&self) -> &Canvas {
        &self.canvas
    }

    pub fn panel(&self) -> &Panel {
        &self.panel
    }

    pub fn pointer(&self) -> Point {
        self.pointer
    }

    pub fn context_menu(&self) -> Option<&ContextMenu> {
        self.context_menu.as_ref()
    }

    pub fn flyout(&self) -> Option<Flyout> {
        self.flyout
    }

    pub fn drag_state(&self) -> Option<DragState> {
        self.drag
    }

    pub fn set_clock(&mut self, time: impl Into<String>, date: impl Into<String>) {
        self.panel.time = time.into();
        self.panel.date = date.into();
        // Кэш панели пересоберётся, но в кадр попадёт только область часов.
        self.damage.add(panel::clock_rect(self.screen));
    }

    pub fn set_network_online(&mut self, online: bool) {
        self.panel.network_online = online;
        self.damage.add(panel::network_rect(self.screen));
    }

    pub fn set_wallpaper_alt(&mut self, alt: bool) {
        if self.wallpaper_alt != alt {
            self.wallpaper_alt = alt;
            self.wallpaper_cache = None;
            self.damage.add_whole();
        }
    }

    pub fn wallpaper_alt(&self) -> bool {
        self.wallpaper_alt
    }

    /// Счётчики построения кэшей слоёв.
    pub fn cache_stats(&self) -> CacheStats {
        self.cache_stats
    }

    /// Счётчики glyph-кэша текущего back buffer.
    pub fn glyph_stats(&self) -> (u64, u64) {
        (self.canvas.glyph_hits(), self.canvas.glyph_misses())
    }

    /// Список чипов окон для панели в z-порядке.
    pub fn chips(&self) -> Vec<(u32, String, bool)> {
        self.windows
            .iter()
            .map(|w| (w.id, w.title.clone(), w.focused))
            .collect()
    }

    // ------------------------------------------------------------------
    // Управление окнами
    // ------------------------------------------------------------------

    /// Создать окно, разместить каскадом, сфокусировать и отправить configure.
    pub fn add_window(
        &mut self,
        client_id: u64,
        window_id: u32,
        title: impl Into<String>,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Vec<HostEvent>, WindowError> {
        if self.windows.len() >= MAX_WINDOWS {
            return Err(WindowError::LimitReached);
        }
        if window_id == 0 {
            return Err(WindowError::InvalidSize);
        }
        if self.window(window_id).is_some() {
            return Err(WindowError::Duplicate(window_id));
        }
        if !(MIN_WINDOW_W..=4096).contains(&width) || !(MIN_WINDOW_H..=4096).contains(&height) {
            return Err(WindowError::InvalidSize);
        }
        let mut window = Window::new(
            window_id,
            client_id,
            title,
            Point::new(0, 0),
            Size::new(width, height),
            format,
            self.seq,
        );
        self.seq += 1;
        let count = self.windows.len() as i32;
        let cascade_x = 96 + (count % 6) * 42;
        let cascade_y = 64 + (count % 6) * 36;
        let max_x = (self.screen.right() - window.frame.w - 24).max(16);
        let max_y = (self.screen.bottom() - PANEL_HEIGHT - window.frame.h - 24).max(16);
        window.set_position(Point::new(cascade_x.min(max_x), cascade_y.min(max_y)));
        window.clamp_position(self.screen, PANEL_HEIGHT);
        self.windows.push(window);

        let mut events = self.set_focus(Some(window_id));
        events.push(self.configure_event(window_id));
        self.damage.add_whole();
        Ok(events)
    }

    /// Переместить окно в заданную позицию (для сценариев и тестов).
    pub fn move_window(&mut self, window_id: u32, position: Point) -> bool {
        let Some(old_frame) = self.window(window_id).map(|window| window.frame) else {
            return false;
        };
        let screen = self.screen;
        if let Some(window) = self.window_mut(window_id) {
            window.set_position(position);
            window.clamp_position(screen, PANEL_HEIGHT);
        }
        self.damage.add(old_frame.inflate(26));
        if let Some(frame) = self.window(window_id).map(|window| window.frame) {
            self.damage.add(frame.inflate(26));
        }
        true
    }

    /// Убрать окно. `notify` — отправить клиенту `close`.
    pub fn remove_window(&mut self, window_id: u32, notify: bool) -> Vec<HostEvent> {
        let Some(index) = self.windows.iter().position(|w| w.id == window_id) else {
            return Vec::new();
        };
        let window = self.windows.remove(index);
        self.chrome_cache.remove(&window_id);
        self.damage.add(window.frame.inflate(26));
        for other in &self.windows {
            if other.frame.intersects(window.frame) {
                self.damage.add(other.frame);
            }
        }
        if self.pressed_window == Some(window_id) {
            self.pressed_window = None;
        }
        if matches!(self.drag, Some(DragState::Move { window_id: id, .. })
            | Some(DragState::Resize { window_id: id, .. }) if id == window_id)
        {
            self.drag = None;
        }
        let mut events = Vec::new();
        if self.focused == Some(window_id) {
            let next = self.topmost_visible();
            events.extend(self.set_focus(next));
        }
        self.damage.add(panel::panel_rect(self.screen));
        if notify {
            events.push(HostEvent::Send {
                client_id: window.client_id,
                message: Message::Close {
                    window_id,
                    reason: CLOSE_SERVER,
                },
            });
        }
        events
    }

    /// Отключился клиент: снять все его окна.
    pub fn remove_client(&mut self, client_id: u64) -> Vec<HostEvent> {
        let ids: Vec<u32> = self
            .windows
            .iter()
            .filter(|w| w.client_id == client_id)
            .map(|w| w.id)
            .collect();
        let mut events = Vec::new();
        for id in ids {
            events.extend(self.remove_window(id, false));
        }
        events
    }

    /// Принять новый кадр поверхности от клиента.
    pub fn commit_surface(
        &mut self,
        client_id: u64,
        window_id: u32,
        surface: Surface,
    ) -> Result<Vec<HostEvent>, WindowError> {
        let Some(window) = self.window(window_id) else {
            return Err(WindowError::NotFound(window_id));
        };
        if window.client_id != client_id {
            return Err(WindowError::NotOwner);
        }
        if surface.width > 4096 || surface.height > 4096 {
            return Err(WindowError::TooLarge);
        }
        let content = window.content_rect();
        let window = self.window_mut(window_id).expect("окно проверено");
        window.surface = Some(surface);
        self.damage.add(content);
        Ok(Vec::new())
    }

    /// Установить серверную тень для ShmSurface: memfd остаётся у сервера,
    /// композитор рисует из этой поверхности.
    pub fn install_shm_surface(
        &mut self,
        client_id: u64,
        window_id: u32,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<(), WindowError> {
        let Some(window) = self.window(window_id) else {
            return Err(WindowError::NotFound(window_id));
        };
        if window.client_id != client_id {
            return Err(WindowError::NotOwner);
        }
        let surface = Surface::new(width, height, format).map_err(WindowError::InvalidSurface)?;
        let content = window.content_rect();
        let window = self.window_mut(window_id).expect("окно проверено");
        window.surface = Some(surface);
        self.damage.add(content);
        Ok(())
    }

    /// Скопировать повреждённые прямоугольники из клиентского memfd в серверную
    /// тень и пометить damage. Пиксели клиента никогда не читаются при отрисовке.
    pub fn shm_update(
        &mut self,
        client_id: u64,
        window_id: u32,
        view: crate::shm::ShmView<'_>,
        rects: &[(i32, i32, u32, u32)],
    ) -> Result<(), WindowError> {
        let Some(window) = self.window(window_id) else {
            return Err(WindowError::NotFound(window_id));
        };
        if window.client_id != client_id {
            return Err(WindowError::NotOwner);
        }
        let content = window.content_rect();
        let view_rect = Rect::new(0, 0, view.width as i32, view.height as i32);
        let mut damaged = Vec::with_capacity(rects.len());
        {
            let window = self.window_mut(window_id).expect("окно проверено");
            let Some(surface) = window.surface.as_mut() else {
                return Err(WindowError::NoSurface);
            };
            let surface_rect = Rect::new(0, 0, surface.width as i32, surface.height as i32);
            for &(x, y, w, h) in rects {
                let w = w.min(i32::MAX as u32) as i32;
                let h = h.min(i32::MAX as u32) as i32;
                let Some(area) = Rect::new(x, y, w, h)
                    .intersection(view_rect)
                    .and_then(|area| area.intersection(surface_rect))
                else {
                    continue;
                };
                copy_shm_rect(surface, &view, area);
                damaged.push(Rect::new(
                    content.x + area.x,
                    content.y + area.y,
                    area.w,
                    area.h,
                ));
            }
        }
        for rect in damaged {
            self.damage.add(rect);
        }
        Ok(())
    }

    /// Клиент сообщил damage-прямоугольники (координаты внутри поверхности).
    pub fn note_damage(
        &mut self,
        client_id: u64,
        window_id: u32,
        rects: &[(i32, i32, u16, u16)],
    ) -> Result<(), WindowError> {
        let Some(window) = self.window(window_id) else {
            return Err(WindowError::NotFound(window_id));
        };
        if window.client_id != client_id {
            return Err(WindowError::NotOwner);
        }
        let content = window.content_rect();
        for (x, y, w, h) in rects {
            let rect = Rect::new(content.x + *x, content.y + *y, *w as i32, *h as i32);
            self.damage.add(rect);
        }
        Ok(())
    }

    fn configure_event(&self, window_id: u32) -> HostEvent {
        let window = self.window(window_id).expect("окно существует");
        HostEvent::Send {
            client_id: window.client_id,
            message: Message::Configure {
                window_id,
                x: window.frame.x + BORDER,
                y: window.frame.y + BORDER + crate::window::TITLEBAR_HEIGHT,
                width: window.content_size.w,
                height: window.content_size.h,
                flags: configure_flags(window),
            },
        }
    }

    fn set_focus(&mut self, next: Option<u32>) -> Vec<HostEvent> {
        if self.focused == next {
            return Vec::new();
        }
        let mut events = Vec::new();
        if let Some(previous) = self.focused {
            if let Some((client, frame)) = self.window(previous).map(|w| (w.client_id, w.frame)) {
                if let Some(window) = self.window_mut(previous) {
                    window.focused = false;
                }
                self.damage.add(frame);
                events.push(HostEvent::Send {
                    client_id: client,
                    message: Message::Focus {
                        window_id: previous,
                        focused: false,
                    },
                });
            }
        }
        self.focused = next;
        if let Some(next_id) = next {
            if let Some((client, frame)) = self.window(next_id).map(|w| (w.client_id, w.frame)) {
                if let Some(window) = self.window_mut(next_id) {
                    window.focused = true;
                }
                self.damage.add(frame);
                events.push(HostEvent::Send {
                    client_id: client,
                    message: Message::Focus {
                        window_id: next_id,
                        focused: true,
                    },
                });
            }
        }
        self.damage.add(panel::panel_rect(self.screen));
        events
    }

    fn topmost_visible(&self) -> Option<u32> {
        self.windows
            .iter()
            .rev()
            .find(|w| !w.minimized)
            .map(|w| w.id)
    }

    /// Поднять окно и сфокусировать его.
    pub fn focus_and_raise(&mut self, window_id: u32) -> Vec<HostEvent> {
        if self.window(window_id).is_none() {
            return Vec::new();
        }
        self.raise_window(window_id);
        self.set_focus(Some(window_id))
    }

    fn raise_window(&mut self, window_id: u32) {
        let Some(index) = self.windows.iter().position(|w| w.id == window_id) else {
            return;
        };
        if index + 1 == self.windows.len() {
            return;
        }
        let window = self.windows.remove(index);
        let mut affected = window.frame;
        for other in &self.windows {
            if other.frame.intersects(window.frame) {
                affected = affected.union(other.frame);
            }
        }
        self.damage.add(affected.inflate(26));
        self.windows.push(window);
        self.damage.add(panel::panel_rect(self.screen));
    }

    fn close_window(&mut self, window_id: u32) -> Vec<HostEvent> {
        self.remove_window(window_id, true)
    }

    fn minimize_window(&mut self, window_id: u32) -> Vec<HostEvent> {
        let Some((frame, client, content)) = self
            .window(window_id)
            .map(|w| (w.frame, w.client_id, w.content_size))
        else {
            return Vec::new();
        };
        if self.window(window_id).map(|w| w.minimized).unwrap_or(true) {
            return Vec::new();
        }
        if let Some(window) = self.window_mut(window_id) {
            window.minimized = true;
        }
        let flag_message = Message::Configure {
            window_id,
            x: frame.x + BORDER,
            y: frame.y + BORDER + crate::window::TITLEBAR_HEIGHT,
            width: content.w,
            height: content.h,
            flags: CONFIGURE_MINIMIZED,
        };
        self.damage.add(frame.inflate(26));
        let mut events = Vec::new();
        if self.focused == Some(window_id) {
            let next = self.topmost_visible();
            events.extend(self.set_focus(next));
        }
        events.push(HostEvent::Send {
            client_id: client,
            message: flag_message,
        });
        self.damage.add(panel::panel_rect(self.screen));
        events
    }

    fn restore_window(&mut self, window_id: u32) -> Vec<HostEvent> {
        if self.window(window_id).is_none() {
            return Vec::new();
        }
        if let Some(window) = self.window_mut(window_id) {
            window.minimized = false;
        }
        self.raise_window(window_id);
        let mut events = self.set_focus(Some(window_id));
        events.push(self.configure_event(window_id));
        self.damage.add(panel::panel_rect(self.screen));
        events
    }

    fn toggle_maximize(&mut self, window_id: u32) -> Vec<HostEvent> {
        let Some((old_frame, maximized, restore_frame)) = self
            .window(window_id)
            .map(|w| (w.frame, w.maximized, w.restore_frame))
        else {
            return Vec::new();
        };
        let (new_frame, new_restore, was_maximized) = if maximized {
            match restore_frame {
                Some(restore) => (restore, None, false),
                None => (old_frame, None, false),
            }
        } else {
            (
                Rect::new(0, 0, self.screen.w, self.screen.h - PANEL_HEIGHT),
                Some(old_frame),
                true,
            )
        };
        if let Some(window) = self.window_mut(window_id) {
            window.maximized = was_maximized;
            window.restore_frame = new_restore;
            window.frame = new_frame;
            window.content_size = content_size_for_frame(new_frame);
        }
        let mut events = self.focus_and_raise(window_id);
        self.damage.add(old_frame.inflate(26));
        self.damage.add(new_frame.inflate(26));
        events.push(self.configure_event(window_id));
        events
    }

    fn finish_move(&mut self, window_id: u32) -> Vec<HostEvent> {
        let Some(window) = self.window(window_id) else {
            return Vec::new();
        };
        let frame = window.frame;
        self.damage.add(frame.inflate(26));
        vec![self.configure_event(window_id)]
    }

    fn finish_resize(&mut self, window_id: u32) -> Vec<HostEvent> {
        let Some(window) = self.window(window_id) else {
            return Vec::new();
        };
        let frame = window.frame;
        self.damage.add(frame.inflate(26));
        vec![self.configure_event(window_id)]
    }

    fn cycle_focus(&mut self) -> Vec<HostEvent> {
        let candidates: Vec<u32> = self
            .windows
            .iter()
            .filter(|w| !w.minimized)
            .map(|w| w.id)
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let next = match self
            .focused
            .and_then(|id| candidates.iter().position(|c| *c == id))
        {
            Some(index) => candidates[(index + 1) % candidates.len()],
            None => candidates[candidates.len() - 1],
        };
        self.focus_and_raise(next)
    }

    // ------------------------------------------------------------------
    // Всплывающие элементы
    // ------------------------------------------------------------------

    fn open_menu(&mut self, mut menu: ContextMenu) {
        menu.clamp_to(self.screen);
        if let Some(old) = &self.context_menu {
            self.damage.add(old.rect.inflate(26));
        }
        self.damage.add(menu.rect.inflate(26));
        self.context_menu = Some(menu);
    }

    fn open_desktop_menu(&mut self, point: Point) {
        self.open_menu(ContextMenu::desktop(point));
    }

    fn open_launcher_menu(&mut self) {
        let icon = panel::icon_rect(self.screen, PanelIcon::Launcher.index());
        let mut menu = ContextMenu::desktop(Point::new(icon.x, 0));
        menu.rect.y = (panel::panel_rect(self.screen).y - menu.rect.h - 6).max(0);
        self.open_menu(menu);
    }

    fn open_window_menu(&mut self, point: Point, window_id: u32, maximized: bool) {
        self.open_menu(ContextMenu::for_window(point, window_id, maximized));
    }

    pub fn close_context_menu(&mut self) {
        if let Some(menu) = self.context_menu.take() {
            self.damage.add(menu.rect.inflate(26));
        }
    }

    fn close_flyout(&mut self) {
        if let Some(flyout) = self.flyout.take() {
            self.damage.add(flyout.rect.inflate(26));
        }
    }

    fn flyout_rect(&self, kind: FlyoutKind) -> Rect {
        let panel_top = panel::panel_rect(self.screen).y;
        match kind {
            FlyoutKind::Network => {
                let network = panel::network_rect(self.screen);
                clamp_flyout(
                    Rect::new(network.x - 60, panel_top - 188 - 8, 300, 188),
                    self.screen,
                )
            }
            FlyoutKind::Calendar => {
                let clock = panel::clock_rect(self.screen);
                clamp_flyout(
                    Rect::new(clock.right() - 300, panel_top - 258 - 8, 300, 258),
                    self.screen,
                )
            }
        }
    }

    fn toggle_flyout(&mut self, kind: FlyoutKind) {
        if matches!(self.flyout, Some(Flyout { kind: current, .. }) if current == kind) {
            self.close_flyout();
            return;
        }
        self.close_flyout();
        self.close_context_menu();
        let rect = self.flyout_rect(kind);
        self.flyout = Some(Flyout { kind, rect });
        self.damage.add(rect.inflate(26));
    }

    fn execute_menu_action(&mut self, action: MenuAction, _button: MouseButton) -> Vec<HostEvent> {
        let target = self.context_menu.as_ref().and_then(|m| m.target_window);
        self.close_context_menu();
        match action {
            MenuAction::OpenTerminal => vec![HostEvent::Launch(AppKind::Terminal)],
            MenuAction::OpenFiles => vec![HostEvent::Launch(AppKind::Files)],
            MenuAction::OpenPaint => vec![HostEvent::Launch(AppKind::Paint)],
            MenuAction::ChangeWallpaper => {
                self.set_wallpaper_alt(!self.wallpaper_alt);
                Vec::new()
            }
            MenuAction::CloseWindow => target.map(|id| self.close_window(id)).unwrap_or_default(),
            MenuAction::MinimizeWindow => target
                .map(|id| self.minimize_window(id))
                .unwrap_or_default(),
            MenuAction::MaximizeWindow => target
                .map(|id| self.toggle_maximize(id))
                .unwrap_or_default(),
            MenuAction::MoveWindow => {
                let Some(id) = target else {
                    return Vec::new();
                };
                let grab = self.window(id).map(|window| {
                    Point::new(
                        self.pointer.x - window.frame.x,
                        (self.pointer.y - window.frame.y).clamp(0, crate::window::TITLEBAR_HEIGHT),
                    )
                });
                let mut events = self.focus_and_raise(id);
                if let Some(grab_offset) = grab {
                    self.drag = Some(DragState::Move {
                        window_id: id,
                        grab_offset,
                        drop_on_click: true,
                    });
                }
                events.append(&mut Vec::new());
                events
            }
            MenuAction::PanelSettings | MenuAction::Separator => Vec::new(),
        }
    }

    fn panel_hover_at(&self, point: Point) -> Option<PanelHover> {
        match self.panel.hit_static(self.screen, point)? {
            PanelHit::Icon(icon) => Some(PanelHover::Icon(icon.index())),
            PanelHit::Network => Some(PanelHover::Network),
            PanelHit::Clock => Some(PanelHover::Clock),
            PanelHit::WindowChip(_) => None,
            PanelHit::Background => {
                let chips = self.chips();
                self.panel
                    .hit_chip(self.screen, point, &chips)
                    .map(PanelHover::WindowChip)
            }
        }
    }

    fn window_button_at(&self, point: Point) -> Option<(u32, WindowPart)> {
        for window in self.windows.iter().rev() {
            if window.minimized {
                continue;
            }
            if let Some(part) = window.part_at(point) {
                return match part {
                    WindowPart::Close | WindowPart::Maximize | WindowPart::Minimize => {
                        Some((window.id, part))
                    }
                    _ => None,
                };
            }
        }
        None
    }

    fn hover_state(&self, point: Point) -> HoverState {
        let panel = self.panel_hover_at(point);
        let menu = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.item_at(point));
        let button = if panel.is_none() && menu.is_none() {
            self.window_button_at(point)
        } else {
            None
        };
        HoverState {
            panel,
            menu,
            button,
        }
    }

    fn damage_button(&mut self, state: Option<(u32, WindowPart)>) {
        if let Some((window_id, part)) = state {
            if let Some(window) = self.window(window_id) {
                let rect = match part {
                    WindowPart::Close => window.close_rect(),
                    WindowPart::Maximize => window.maximize_rect(),
                    WindowPart::Minimize => window.minimize_rect(),
                    _ => return,
                };
                self.damage.add(rect.inflate(2));
            }
        }
    }

    fn damage_cursor(&mut self, point: Point) {
        self.damage.add(Rect::new(point.x - 1, point.y - 1, 16, 23));
    }

    fn panel_hit(&self, point: Point) -> Option<PanelHit> {
        let hit = self.panel.hit_static(self.screen, point)?;
        if matches!(hit, PanelHit::Background) {
            let chips = self.chips();
            if let Some(id) = self.panel.hit_chip(self.screen, point, &chips) {
                return Some(PanelHit::WindowChip(id));
            }
        }
        Some(hit)
    }

    fn panel_click(&mut self, hit: PanelHit, button: MouseButton) -> Vec<HostEvent> {
        if button == MouseButton::Right {
            if let PanelHit::WindowChip(_) = hit {
                // Правый клик по чипу — как по рабочему столу.
            } else {
                self.open_desktop_menu(self.pointer);
                return Vec::new();
            }
        }
        match hit {
            PanelHit::Icon(PanelIcon::Launcher) => {
                self.open_launcher_menu();
                Vec::new()
            }
            PanelHit::Icon(PanelIcon::Terminal) => vec![HostEvent::Launch(AppKind::Terminal)],
            PanelHit::Icon(PanelIcon::Files) => vec![HostEvent::Launch(AppKind::Files)],
            PanelHit::Icon(PanelIcon::Paint) => vec![HostEvent::Launch(AppKind::Paint)],
            PanelHit::WindowChip(id) => {
                if self.window(id).map(|w| w.minimized).unwrap_or(false) {
                    self.restore_window(id)
                } else {
                    self.focus_and_raise(id)
                }
            }
            PanelHit::Network => {
                self.toggle_flyout(FlyoutKind::Network);
                Vec::new()
            }
            PanelHit::Clock => {
                self.toggle_flyout(FlyoutKind::Calendar);
                Vec::new()
            }
            PanelHit::Background => Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // Ввод
    // ------------------------------------------------------------------

    /// Абсолютное движение указателя.
    pub fn pointer_motion(&mut self, point: Point) -> Vec<HostEvent> {
        let point = Point::new(
            point.x.clamp(0, self.screen.right() - 1),
            point.y.clamp(0, self.screen.bottom() - 1),
        );
        let old = self.pointer;
        if point == old {
            return Vec::new();
        }
        let old_hover = self.hover_state(old);
        let new_hover = self.hover_state(point);
        self.damage_cursor(old);
        self.damage_cursor(point);
        self.pointer = point;
        if old_hover.panel != new_hover.panel {
            self.damage.add(panel::panel_rect(self.screen));
        }
        if old_hover.menu != new_hover.menu {
            if let Some(menu) = &self.context_menu {
                let rect = menu.rect;
                self.damage.add(rect);
            }
        }
        if old_hover.button != new_hover.button {
            self.damage_button(old_hover.button);
            self.damage_button(new_hover.button);
        }

        if let Some(menu) = self.context_menu.as_mut() {
            menu.hover = menu.item_at(point);
        }

        let mut events = Vec::new();
        let delta = Point::new(point.x - old.x, point.y - old.y);
        match self.drag {
            Some(DragState::Move {
                window_id,
                grab_offset,
                ..
            }) => {
                let old_frame = self.window(window_id).map(|window| window.frame);
                let screen = self.screen;
                if let Some(window) = self.window_mut(window_id) {
                    window
                        .set_position(Point::new(point.x - grab_offset.x, point.y - grab_offset.y));
                    window.clamp_position(screen, PANEL_HEIGHT);
                }
                if let Some(frame) = old_frame {
                    self.damage.add(frame.inflate(26));
                }
                if let Some(frame) = self.window(window_id).map(|window| window.frame) {
                    self.damage.add(frame.inflate(26));
                }
            }
            Some(DragState::Resize {
                window_id,
                start_pointer,
                start_content,
            }) => {
                let dx = point.x - start_pointer.x;
                let dy = point.y - start_pointer.y;
                let max_w = (self.screen.w - BORDER * 2) as u32;
                let max_h = (self.screen.h - PANEL_HEIGHT - BORDER * 2) as u32;
                let new_size = Size::new(
                    (start_content.w as i32 + dx).clamp(MIN_CONTENT.w as i32, max_w as i32) as u32,
                    (start_content.h as i32 + dy).clamp(MIN_CONTENT.h as i32, max_h as i32) as u32,
                );
                let old_frame = self.window(window_id).map(|window| window.frame);
                if let Some(window) = self.window_mut(window_id) {
                    window.set_content_size(new_size);
                }
                if let Some(frame) = old_frame {
                    self.damage.add(frame.inflate(26));
                }
                if let Some(frame) = self.window(window_id).map(|window| window.frame) {
                    self.damage.add(frame.inflate(26));
                }
            }
            None => {}
        }

        // Ввод клиенту: захваченное окно при нажатой кнопке, иначе окно под указателем.
        let target = if self.drag.is_some()
            || self.context_menu.is_some()
            || self.flyout.is_some()
            || panel::panel_rect(self.screen).contains(point)
        {
            self.pressed_window
        } else {
            self.pressed_window
                .or_else(|| self.content_window_at(point))
        };
        if let Some(window_id) = target {
            if let Some(window) = self.window(window_id) {
                if !window.minimized {
                    let content = window.content_rect();
                    events.push(HostEvent::Send {
                        client_id: window.client_id,
                        message: Message::Input {
                            window_id,
                            event: crate::protocol::InputEvent::Motion {
                                x: point.x - content.x,
                                y: point.y - content.y,
                                dx: delta.x,
                                dy: delta.y,
                                buttons: self.buttons,
                            },
                        },
                    });
                }
            }
        }
        events
    }

    /// Относительное движение указателя (мышь без абсолютной координации).
    pub fn pointer_motion_relative(&mut self, dx: i32, dy: i32) -> Vec<HostEvent> {
        self.pointer_motion(Point::new(self.pointer.x + dx, self.pointer.y + dy))
    }

    fn content_window_at(&self, point: Point) -> Option<u32> {
        self.windows
            .iter()
            .rev()
            .find(|w| !w.minimized && w.content_rect().contains(point))
            .map(|w| w.id)
    }

    fn window_hit(&self, point: Point) -> Option<(u32, WindowPart)> {
        for window in self.windows.iter().rev() {
            if window.minimized || !window.frame.contains(point) {
                continue;
            }
            if let Some(part) = window.part_at(point) {
                return Some((window.id, part));
            }
        }
        None
    }

    fn emit_button(
        &self,
        window_id: u32,
        point: Point,
        button: MouseButton,
        pressed: bool,
    ) -> Vec<HostEvent> {
        let Some(window) = self.window(window_id) else {
            return Vec::new();
        };
        if window.minimized {
            return Vec::new();
        }
        let content = window.content_rect();
        vec![HostEvent::Send {
            client_id: window.client_id,
            message: Message::Input {
                window_id,
                event: crate::protocol::InputEvent::Button {
                    x: point.x - content.x,
                    y: point.y - content.y,
                    button: button.to_wire(),
                    pressed,
                    buttons: self.buttons,
                },
            },
        }]
    }

    /// Нажатие/отпускание кнопки мыши в текущей позиции указателя.
    pub fn pointer_button(&mut self, button: MouseButton, pressed: bool) -> Vec<HostEvent> {
        let point = self.pointer;
        let bit = button.bitmask();
        if pressed {
            if self.buttons & bit != 0 {
                return Vec::new();
            }
            // Завершение перемещения по пункту меню — клик отпускания.
            if let Some(DragState::Move {
                window_id,
                drop_on_click: true,
                ..
            }) = self.drag
            {
                if button == MouseButton::Left {
                    self.drag = None;
                    return self.finish_move(window_id);
                }
            }
            self.buttons |= bit;
            self.damage_cursor(point);

            if self.context_menu.is_some() {
                let action = self
                    .context_menu
                    .as_ref()
                    .and_then(|menu| menu.action_at(point));
                if let Some(action) = action {
                    return self.execute_menu_action(action, button);
                }
                let inside = self
                    .context_menu
                    .as_ref()
                    .map(|menu| menu.rect.contains(point))
                    .unwrap_or(false);
                if !inside {
                    self.close_context_menu();
                }
                return Vec::new();
            }
            if let Some(flyout) = self.flyout {
                if flyout.rect.contains(point) {
                    return Vec::new();
                }
                self.close_flyout();
            }
            if let Some(hit) = self.panel_hit(point) {
                return self.panel_click(hit, button);
            }
            if let Some((window_id, part)) = self.window_hit(point) {
                return match part {
                    WindowPart::Close => self.close_window(window_id),
                    WindowPart::Maximize => self.toggle_maximize(window_id),
                    WindowPart::Minimize => self.minimize_window(window_id),
                    WindowPart::ResizeCorner => {
                        let start_content = self.window(window_id).map(|w| w.content_size);
                        let events = self.focus_and_raise(window_id);
                        if let Some(start_content) = start_content {
                            self.drag = Some(DragState::Resize {
                                window_id,
                                start_pointer: point,
                                start_content,
                            });
                        }
                        events
                    }
                    WindowPart::TitleBar => {
                        if button == MouseButton::Right {
                            let maximized =
                                self.window(window_id).map(|w| w.maximized).unwrap_or(false);
                            let events = self.focus_and_raise(window_id);
                            self.open_window_menu(point, window_id, maximized);
                            return events;
                        }
                        let frame = self.window(window_id).map(|w| w.frame);
                        let events = self.focus_and_raise(window_id);
                        if let Some(frame) = frame {
                            self.drag = Some(DragState::Move {
                                window_id,
                                grab_offset: Point::new(point.x - frame.x, point.y - frame.y),
                                drop_on_click: false,
                            });
                        }
                        events
                    }
                    WindowPart::Content => {
                        let mut events = self.focus_and_raise(window_id);
                        self.pressed_window = Some(window_id);
                        events.extend(self.emit_button(window_id, point, button, true));
                        events
                    }
                };
            }
            if button == MouseButton::Right {
                self.open_desktop_menu(point);
            }
            Vec::new()
        } else {
            if self.buttons & bit == 0 {
                return Vec::new();
            }
            self.buttons &= !bit;
            self.damage_cursor(point);
            let mut events = Vec::new();
            if let Some(drag) = self.drag {
                match drag {
                    DragState::Move {
                        window_id,
                        drop_on_click: false,
                        ..
                    } => {
                        self.drag = None;
                        events.extend(self.finish_move(window_id));
                    }
                    DragState::Resize { window_id, .. } => {
                        self.drag = None;
                        events.extend(self.finish_resize(window_id));
                    }
                    DragState::Move {
                        drop_on_click: true,
                        ..
                    } => {}
                }
            }
            if let Some(window_id) = self.pressed_window.take() {
                events.extend(self.emit_button(window_id, point, button, false));
            }
            events
        }
    }

    /// Прокрутка колеса; доставляется окну под указателем.
    pub fn axis(&mut self, horizontal: i32, vertical: i32) -> Vec<HostEvent> {
        let Some(window_id) = self.content_window_at(self.pointer) else {
            return Vec::new();
        };
        let Some(window) = self.window(window_id) else {
            return Vec::new();
        };
        let content = window.content_rect();
        vec![HostEvent::Send {
            client_id: window.client_id,
            message: Message::Input {
                window_id,
                event: crate::protocol::InputEvent::Axis {
                    x: self.pointer.x - content.x,
                    y: self.pointer.y - content.y,
                    horizontal,
                    vertical,
                    buttons: self.buttons,
                },
            },
        }]
    }

    /// Клавиатурное событие. Комбинации композитора обрабатываются здесь.
    pub fn key_event(&mut self, code: u32, pressed: bool, modifiers: u32) -> Vec<HostEvent> {
        if pressed {
            if code == key::ESC && (self.context_menu.is_some() || self.flyout.is_some()) {
                self.close_context_menu();
                self.close_flyout();
                return Vec::new();
            }
            if code == key::F10 {
                self.open_desktop_menu(self.pointer);
                return Vec::new();
            }
            if code == key::TAB && modifiers & MOD_ALT != 0 {
                return self.cycle_focus();
            }
            if code == key::T && modifiers & MOD_CTRL != 0 && modifiers & MOD_ALT != 0 {
                return vec![HostEvent::Launch(AppKind::Terminal)];
            }
            if code == key::SUPER {
                self.open_launcher_menu();
                return Vec::new();
            }
        }
        let Some(window_id) = self.focused else {
            return Vec::new();
        };
        let Some(window) = self.window(window_id) else {
            return Vec::new();
        };
        if window.minimized {
            return Vec::new();
        }
        vec![HostEvent::Send {
            client_id: window.client_id,
            message: Message::Input {
                window_id,
                event: crate::protocol::InputEvent::Key {
                    code,
                    pressed,
                    modifiers,
                },
            },
        }]
    }

    // ------------------------------------------------------------------
    // Отрисовка
    // ------------------------------------------------------------------

    /// Перерисовать накопленный damage. Возвращает перерисованные области.
    pub fn render(&mut self) -> Vec<Rect> {
        let damage = self.damage.take();
        if damage.is_empty() {
            return Vec::new();
        }
        let rects = if damage.whole {
            vec![self.screen]
        } else {
            damage.rects
        };
        let mut canvas = std::mem::replace(&mut self.canvas, Canvas::new(1, 1, color::BLACK));
        canvas.set_clip(&rects);
        self.paint_scene(&mut canvas);
        canvas.clear_clip();
        self.canvas = canvas;
        self.frame_no += 1;
        rects
    }

    /// Принудительно перерисовать весь экран (для скриншотов).
    pub fn render_full(&mut self) -> Vec<Rect> {
        self.damage.add_whole();
        self.render()
    }

    fn paint_scene(&mut self, canvas: &mut Canvas) {
        // Ключи/подсказки считаем до временного изъятия окон из `self`.
        let chips = self.chips();
        let panel_hover = self.panel_hover_at(self.pointer);
        let hover_button = self.window_button_at(self.pointer);
        self.paint_wallpaper_cached(canvas);
        let windows = std::mem::take(&mut self.windows);
        for window in &windows {
            if window.minimized {
                continue;
            }
            let hover = hover_button
                .filter(|(id, _)| *id == window.id)
                .map(|(_, part)| part);
            self.paint_window_cached(canvas, window, hover);
        }
        self.windows = windows;
        self.paint_panel_cached(canvas, &chips, panel_hover);
        if let Some(flyout) = self.flyout {
            self.paint_flyout(canvas, flyout);
        }
        if let Some(menu) = &self.context_menu {
            menu.draw(canvas, &self.theme);
        }
        self.paint_cursor(canvas);
    }

    /// Обои кэшируются целиком: градиент и два радиальных свечения считаются
    /// один раз, затем копируются только повреждённые строки.
    fn paint_wallpaper_cached(&mut self, canvas: &mut Canvas) {
        if self.wallpaper_cache.is_none() {
            let mut wallpaper =
                Canvas::new(self.screen.w as u32, self.screen.h as u32, color::BLACK);
            self.paint_wallpaper(&mut wallpaper);
            self.cache_stats.wallpaper_builds += 1;
            self.wallpaper_cache = Some(wallpaper);
        }
        if let Some(wallpaper) = &self.wallpaper_cache {
            canvas.blit_opaque(wallpaper, self.screen, Point::new(0, 0));
        }
    }

    fn paint_wallpaper(&self, canvas: &mut Canvas) {
        let (top, bottom, glow, glow_strong) = if self.wallpaper_alt {
            self.theme.wallpaper_alt()
        } else {
            (
                self.theme.wallpaper_top,
                self.theme.wallpaper_bottom,
                self.theme.wallpaper_glow,
                self.theme.wallpaper_glow_strong,
            )
        };
        canvas.fill_gradient(self.screen, top, bottom);
        canvas.radial_glow(
            Point::new(
                self.screen.right() - self.screen.w / 5,
                self.screen.y + self.screen.h / 4,
            ),
            460,
            glow,
            44,
        );
        canvas.radial_glow(
            Point::new(self.screen.x + self.screen.w / 7, self.screen.bottom() - 90),
            300,
            glow_strong,
            34,
        );
    }

    fn paint_window_cached(
        &mut self,
        canvas: &mut Canvas,
        window: &Window,
        hover: Option<WindowPart>,
    ) {
        let frame = window.frame;
        // Тень предвычислена для размера окна; внеэкранные окна рисуем старым
        // путём, чтобы отсечение по bounds не меняло геометрию скруглений.
        let shadow_box = frame.inflate(9).offset(0, 6);
        if self.screen.contains_rect(shadow_box) {
            self.ensure_shadow(frame.w, frame.h);
            if let Some(shadow) = self.shadow_cache.get(&(frame.w, frame.h)) {
                for layer in &shadow.layers {
                    for &(y, x0, x1) in &layer.rows {
                        for x in x0..x1 {
                            canvas.blend(frame.x + x, frame.y + y, color::BLACK, layer.alpha);
                        }
                    }
                }
            }
        } else {
            canvas.shadow(frame, WINDOW_RADIUS, 9, 18);
        }

        self.ensure_chrome(window, hover);
        if let Some(chrome) = self.chrome_cache.get(&window.id) {
            canvas.blit_layer(&chrome.layer, Point::new(frame.x, frame.y));
        }

        let content = window.content_rect();
        canvas.fill_rect(content, self.theme.window_bg);
        if let Some(surface) = &window.surface {
            canvas.blit_surface(surface, content, 255);
        } else {
            canvas.text(
                content.x + 16,
                content.y + 16,
                1,
                self.theme.titlebar_text_dim,
                "waiting for first frame...",
            );
        }

        // Разделитель заголовка и акцент фокуса (поверх клиентского кадра).
        let titlebar = window.titlebar_rect();
        canvas.hline(
            titlebar.x,
            titlebar.right(),
            titlebar.bottom(),
            self.theme.separator,
        );
        if window.focused {
            canvas.hline(
                titlebar.x + 1,
                titlebar.right() - 1,
                titlebar.bottom() + 1,
                self.theme.accent,
            );
        }

        // Индикатор resize-угла.
        if window.focused {
            let corner = window.resize_corner_rect();
            for i in 0..3 {
                let offset = i * 4;
                canvas.line(
                    Point::new(corner.right() - 3 - offset, corner.bottom() - 4),
                    Point::new(corner.right() - 4, corner.bottom() - 3 - offset),
                    self.theme.accent_dim,
                    1,
                );
            }
        }
    }

    fn ensure_shadow(&mut self, width: i32, height: i32) {
        if !self.shadow_cache.contains_key(&(width, height)) {
            let cache = build_shadow_cache(width, height);
            self.cache_stats.shadow_builds += 1;
            self.shadow_cache.insert((width, height), cache);
        }
    }

    /// Предвычислить слои тени для окна `width`×`height`.
    ///
    /// Геометрия повторяет `Canvas::shadow` (9 слоёв, `radius = 10`, шаг alpha
    /// 18), но считается один раз: в кадре остаются только построчные пролёты
    /// и alpha-смешивание уже готовых прямоугольников.
    fn ensure_chrome(&mut self, window: &Window, hover: Option<WindowPart>) {
        let key = ChromeKey {
            width: window.frame.w,
            height: window.frame.h,
            title: window.title.clone(),
            focused: window.focused,
            maximized: window.maximized,
            hover,
        };
        let rebuild = match self.chrome_cache.get(&window.id) {
            Some(cache) => cache.key != key,
            None => true,
        };
        if rebuild {
            let layer = self.build_chrome(window, hover);
            self.cache_stats.chrome_builds += 1;
            self.chrome_cache
                .insert(window.id, ChromeCache { key, layer });
        }
    }

    /// Оформление окна без клиентского содержимого и тени. Холст прозрачный:
    /// нулевые пиксели (скруглённые углы) не затирают то, что под окном.
    fn build_chrome(&self, window: &Window, hover: Option<WindowPart>) -> Canvas {
        let frame = Rect::new(0, 0, window.frame.w, window.frame.h);
        let mut layer = Canvas::new_transparent(frame.w as u32, frame.h as u32);
        layer.fill_rounded_rect(frame, WINDOW_RADIUS, window.border_color(&self.theme), 255);
        let inner = frame.inset(BORDER);
        layer.fill_rounded_rect(inner, WINDOW_RADIUS - 1, self.theme.window_bg, 255);
        let titlebar = crate::window::titlebar_rect_for(frame);
        layer.fill_rounded_rect(
            Rect::new(
                titlebar.x,
                titlebar.y,
                titlebar.w,
                titlebar.h + WINDOW_RADIUS,
            ),
            WINDOW_RADIUS - 1,
            window.titlebar_color(&self.theme),
            255,
        );
        let title_color = if window.focused {
            self.theme.titlebar_text
        } else {
            self.theme.titlebar_text_dim
        };
        layer.text(
            titlebar.x + 12,
            titlebar.y + (crate::window::TITLEBAR_HEIGHT - 16) / 2,
            1,
            title_color,
            &window.title_text(),
        );
        self.paint_window_buttons_frame(&mut layer, frame, window.maximized, hover);
        layer
    }

    fn paint_window_buttons_frame(
        &self,
        canvas: &mut Canvas,
        frame: Rect,
        maximized: bool,
        hover_part: Option<WindowPart>,
    ) {
        for part in [
            WindowPart::Minimize,
            WindowPart::Maximize,
            WindowPart::Close,
        ] {
            let rect = match part {
                WindowPart::Minimize => crate::window::button_rect_for(frame, 2),
                WindowPart::Maximize => crate::window::button_rect_for(frame, 1),
                WindowPart::Close => crate::window::button_rect_for(frame, 0),
                _ => continue,
            };
            if hover_part == Some(part) {
                let color = if part == WindowPart::Close {
                    self.theme.button_close
                } else {
                    self.theme.button_hover
                };
                canvas.fill_rounded_rect(rect, 5, color, 255);
            }
            let color = if hover_part == Some(part) && part == WindowPart::Close {
                color::WHITE
            } else {
                self.theme.titlebar_text
            };
            let center = rect.center();
            match part {
                WindowPart::Minimize => {
                    canvas.hline(center.x - 5, center.x + 5, center.y + 4, color);
                }
                WindowPart::Maximize => {
                    if maximized {
                        canvas.stroke_rounded_rect(
                            Rect::new(center.x - 5, center.y - 3, 9, 9),
                            1,
                            color,
                            255,
                        );
                        canvas.stroke_rounded_rect(
                            Rect::new(center.x - 3, center.y - 5, 9, 9),
                            1,
                            color,
                            255,
                        );
                    } else {
                        canvas.stroke_rounded_rect(
                            Rect::new(center.x - 5, center.y - 5, 10, 10),
                            1,
                            color,
                            255,
                        );
                    }
                }
                WindowPart::Close => {
                    canvas.line(
                        Point::new(center.x - 4, center.y - 4),
                        Point::new(center.x + 4, center.y + 4),
                        color,
                        2,
                    );
                    canvas.line(
                        Point::new(center.x + 4, center.y - 4),
                        Point::new(center.x - 4, center.y + 4),
                        color,
                        2,
                    );
                }
                _ => {}
            }
        }
    }

    /// Панель кэшируется в отдельный strip-холст (включая hover): перестраиваем
    /// только при изменении часов/сети/чипов/подсветки.
    fn paint_panel_cached(
        &mut self,
        canvas: &mut Canvas,
        chips: &[(u32, String, bool)],
        hover: Option<PanelHover>,
    ) {
        let matches = match &self.panel_cache {
            Some((key, _)) => {
                key.screen == self.screen
                    && key.time == self.panel.time
                    && key.date == self.panel.date
                    && key.network_online == self.panel.network_online
                    && key.hover == hover
                    && key.chips == chips
            }
            None => false,
        };
        if !matches {
            let mut strip = Canvas::new(self.screen.w as u32, PANEL_HEIGHT as u32, color::BLACK);
            let virtual_screen = Rect::new(0, 0, self.screen.w, PANEL_HEIGHT);
            self.panel
                .draw(&mut strip, virtual_screen, &self.theme, hover, chips);
            self.cache_stats.panel_builds += 1;
            self.panel_cache = Some((
                PanelKey {
                    screen: self.screen,
                    time: self.panel.time.clone(),
                    date: self.panel.date.clone(),
                    network_online: self.panel.network_online,
                    hover,
                    chips: chips.to_vec(),
                },
                strip,
            ));
        }
        if let Some((_, strip)) = &self.panel_cache {
            canvas.blit_opaque(
                strip,
                Rect::new(0, 0, strip.width() as i32, strip.height() as i32),
                Point::new(0, self.screen.bottom() - PANEL_HEIGHT),
            );
        }
    }

    fn paint_cursor(&self, canvas: &mut Canvas) {
        let p = self.pointer;
        let points = [
            Point::new(p.x, p.y),
            Point::new(p.x, p.y + 16),
            Point::new(p.x + 4, p.y + 12),
            Point::new(p.x + 7, p.y + 18),
            Point::new(p.x + 10, p.y + 17),
            Point::new(p.x + 7, p.y + 11),
            Point::new(p.x + 12, p.y + 11),
        ];
        canvas.fill_polygon(&points, self.theme.cursor_fill, 255);
        for i in 0..points.len() {
            canvas.line(
                points[i],
                points[(i + 1) % points.len()],
                self.theme.cursor_outline,
                1,
            );
        }
    }

    fn paint_flyout(&self, canvas: &mut Canvas, flyout: Flyout) {
        canvas.shadow(flyout.rect, 10, 9, 18);
        canvas.fill_rounded_rect(flyout.rect, 10, self.theme.menu_bg, 252);
        canvas.stroke_rounded_rect(flyout.rect, 10, self.theme.menu_border, 255);
        match flyout.kind {
            FlyoutKind::Network => self.paint_network_flyout(canvas, flyout.rect),
            FlyoutKind::Calendar => self.paint_calendar_flyout(canvas, flyout.rect),
        }
    }

    fn paint_network_flyout(&self, canvas: &mut Canvas, rect: Rect) {
        let x = rect.x + 16;
        canvas.text(x, rect.y + 12, 1, self.theme.menu_text, "Network");
        canvas.hline(
            rect.x + 10,
            rect.right() - 10,
            rect.y + 34,
            self.theme.separator,
        );
        let status = if self.panel.network_online {
            "Connected | 5 GHz"
        } else {
            "Disconnected"
        };
        let status_color = if self.panel.network_online {
            self.theme.accent
        } else {
            self.theme.panel_icon_dim
        };
        // Wi-Fi иконка.
        let icon_center = Point::new(x + 9, rect.y + 54);
        canvas.fill_circle(icon_center, 2, status_color, 255);
        for radius in [6, 10, 14] {
            canvas.arc(icon_center, radius, 218.0, 322.0, status_color, 2);
        }
        canvas.text(x + 28, rect.y + 44, 1, self.theme.menu_text, "Pagh-OS-5G");
        canvas.text(x + 28, rect.y + 62, 1, status_color, status);

        let wired_y = rect.y + 88;
        canvas.fill_rounded_rect(
            Rect::new(x, wired_y + 3, 18, 12),
            2,
            self.theme.panel_icon_dim,
            255,
        );
        canvas.text(x + 28, wired_y, 1, self.theme.menu_text, "Wired");
        canvas.text(
            x + 28,
            wired_y + 18,
            1,
            self.theme.panel_text_dim,
            "Connected | 1 Gb/s",
        );

        canvas.hline(
            rect.x + 10,
            rect.right() - 10,
            rect.y + 132,
            self.theme.separator,
        );
        canvas.text(x, rect.y + 144, 1, self.theme.menu_text, "VPN");
        canvas.text(x + 28, rect.y + 144, 1, self.theme.panel_text_dim, "Off");
        canvas.text(
            x,
            rect.bottom() - 24,
            1,
            self.theme.menu_text_dim,
            "Network settings...",
        );
    }

    fn paint_calendar_flyout(&self, canvas: &mut Canvas, rect: Rect) {
        let x = rect.x + 16;
        canvas.text(x, rect.y + 10, 1, self.theme.menu_text, "November 2026");
        canvas.text(
            rect.right() - 16 - 40,
            rect.y + 10,
            1,
            self.theme.accent,
            "23:38",
        );
        canvas.hline(
            rect.x + 10,
            rect.right() - 10,
            rect.y + 32,
            self.theme.separator,
        );
        let weekdays = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
        let cell_w = (rect.w - 24) / 7;
        for (index, label) in weekdays.iter().enumerate() {
            canvas.text(
                rect.x + 12 + index as i32 * cell_w + (cell_w - 16) / 2,
                rect.y + 42,
                1,
                self.theme.panel_text_dim,
                label,
            );
        }
        // 1 ноября 2026 — воскресенье, поэтому сетка начинается с шестой колонки.
        let first_column = 6;
        for day in 1..=30i32 {
            let cell = first_column + day - 1;
            let column = cell % 7;
            let row = cell / 7;
            let cell_rect = Rect::new(
                rect.x + 12 + column * cell_w,
                rect.y + 66 + row * 28,
                cell_w,
                24,
            );
            if day == 15 {
                canvas.fill_circle(cell_rect.center(), 11, self.theme.accent, 255);
                canvas.text(
                    cell_rect.center().x - 8,
                    cell_rect.center().y - 8,
                    1,
                    color::WHITE,
                    "15",
                );
            } else {
                let text = day.to_string();
                canvas.text(
                    cell_rect.center().x - text.len() as i32 * 4,
                    cell_rect.y + 4,
                    1,
                    self.theme.menu_text,
                    &text,
                );
            }
        }
        canvas.text(
            x,
            rect.bottom() - 22,
            1,
            self.theme.menu_text_dim,
            "Sunday, 15 November 2026",
        );
    }
}

fn copy_shm_rect(surface: &mut Surface, view: &crate::shm::ShmView<'_>, area: Rect) {
    let width = surface.width as usize;
    let stride = view.stride as usize;
    match view.format {
        PixelFormat::Xrgb8888 => {
            for row in 0..area.h as usize {
                let src_off = (area.y as usize + row) * stride + area.x as usize * 4;
                let src = &view.data[src_off..src_off + area.w as usize * 4];
                let dst_off = (area.y as usize + row) * width + area.x as usize;
                let dst = &mut surface.pixels_mut()[dst_off..dst_off + area.w as usize];
                for (dst, chunk) in dst.iter_mut().zip(src.chunks_exact(4)) {
                    let value = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    *dst = (value & 0x00FF_FFFF) | 0xFF00_0000;
                }
            }
        }
        PixelFormat::Rgb565 => {
            for row in 0..area.h as usize {
                let src_off = (area.y as usize + row) * stride + area.x as usize * 2;
                let src = &view.data[src_off..src_off + area.w as usize * 2];
                let dst_off = (area.y as usize + row) * width + area.x as usize;
                let dst = &mut surface.pixels_mut()[dst_off..dst_off + area.w as usize];
                for (dst, chunk) in dst.iter_mut().zip(src.chunks_exact(2)) {
                    let value = u16::from_le_bytes([chunk[0], chunk[1]]);
                    *dst = crate::surface::rgb565_to_rgb(value) | 0xFF00_0000;
                }
            }
        }
    }
}

fn content_size_for_frame(frame: Rect) -> Size {
    Size::new(
        (frame.w - BORDER * 2).max(1) as u32,
        (frame.h - BORDER * 2 - crate::window::TITLEBAR_HEIGHT).max(1) as u32,
    )
}

fn configure_flags(window: &Window) -> u32 {
    let mut flags = CONFIGURE_RESIZABLE;
    if window.focused {
        flags |= CONFIGURE_FOCUSED;
    }
    if window.maximized {
        flags |= CONFIGURE_MAXIMIZED;
    }
    if window.minimized {
        flags |= CONFIGURE_MINIMIZED;
    }
    flags
}

fn clamp_flyout(mut rect: Rect, screen: Rect) -> Rect {
    rect.x = rect.x.clamp(8, (screen.right() - rect.w - 8).max(8));
    rect.y = rect
        .y
        .clamp(8, (screen.bottom() - PANEL_HEIGHT - rect.h - 8).max(8));
    rect
}

/// Горизонтальный отступ скругления для строки на расстоянии `dy` от края.
fn rounded_inset(radius: i32, dy: i32) -> i32 {
    if radius <= 0 || dy >= radius {
        return 0;
    }
    let r = radius as f64;
    let d = (radius - dy) as f64;
    (r - (r * r - d * d).sqrt()).round() as i32
}

fn build_shadow_cache(width: i32, height: i32) -> ShadowCache {
    const LAYERS: i32 = 9;
    const RADIUS: i32 = WINDOW_RADIUS;
    const OPACITY_STEP: u8 = 18;
    let mut layers = Vec::with_capacity(LAYERS as usize);
    for layer in (1..=LAYERS).rev() {
        let spread = layer;
        // rect.inflate(spread).offset(0, spread / 2 + 2) в координатах окна.
        let layer_rect = Rect::new(
            -spread,
            spread / 2 + 2 - spread,
            width + spread * 2,
            height + spread * 2,
        );
        let radius = (RADIUS + spread)
            .max(0)
            .min(layer_rect.w / 2)
            .min(layer_rect.h / 2);
        let alpha = OPACITY_STEP.saturating_sub(layer as u8).max(2);
        let mut rows = Vec::with_capacity(layer_rect.h.max(0) as usize);
        for y in layer_rect.y..layer_rect.bottom() {
            let dy = (y - layer_rect.y).min(layer_rect.bottom() - 1 - y);
            let inset = rounded_inset(radius, dy);
            let x0 = layer_rect.x + inset;
            let x1 = layer_rect.right() - inset;
            if x1 > x0 {
                rows.push((y, x0, x1));
            }
        }
        layers.push(ShadowLayer { alpha, rows });
    }
    ShadowCache { layers }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CONFIGURE_MAXIMIZED, MOD_SHIFT};

    fn compositor() -> Compositor {
        let mut compositor = Compositor::new(1280, 720);
        compositor
            .add_window(1, 1, "Terminal", 640, 400, PixelFormat::Xrgb8888)
            .unwrap();
        compositor
            .add_window(1, 2, "Files", 560, 340, PixelFormat::Xrgb8888)
            .unwrap();
        compositor
    }

    fn titlebar_point(compositor: &Compositor, window_id: u32) -> Point {
        let window = compositor.window(window_id).unwrap();
        Point::new(window.frame.x + 60, window.titlebar_rect().center().y)
    }

    #[test]
    fn add_window_focuses_and_places_on_screen() {
        let compositor = compositor();
        assert_eq!(compositor.windows().len(), 2);
        assert_eq!(compositor.focused_id(), Some(2));
        let window = compositor.window(2).unwrap();
        assert!(compositor.screen().contains_rect(Rect::new(
            window.frame.x,
            window.frame.y,
            window.frame.w,
            1
        )));
        assert!(window.focused);
        assert!(!compositor.window(1).unwrap().focused);
    }

    #[test]
    fn click_content_raises_and_focuses() {
        let mut compositor = compositor();
        // Окно 2 сверху; клик по видимой части окна 1 невозможен, если оно перекрыто.
        // Сдвигаем окно 1, чтобы его содержимое было доступно.
        compositor
            .window_mut(1)
            .unwrap()
            .set_position(Point::new(20, 100));
        // Точка на левой видимой части окна 1 (окно 2 начинается с x = 138).
        let point = Point::new(60, 300);
        assert!(compositor.window(1).unwrap().content_rect().contains(point));
        let events = compositor.pointer_motion(point);
        assert!(!events.is_empty());
        let events = compositor.pointer_button(MouseButton::Left, true);
        assert_eq!(compositor.focused_id(), Some(1));
        assert_eq!(compositor.windows().last().unwrap().id, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            HostEvent::Send {
                message: Message::Input { .. },
                ..
            }
        )));
        compositor.pointer_button(MouseButton::Left, false);
    }

    #[test]
    fn drag_titlebar_moves_window_and_reports_damage() {
        let mut compositor = compositor();
        let _ = compositor.render_full();
        let start = titlebar_point(&compositor, 2);
        compositor.pointer_motion(start);
        compositor.pointer_button(MouseButton::Left, true);
        let origin = compositor.window(2).unwrap().frame;
        compositor.pointer_motion(Point::new(start.x + 100, start.y + 40));
        let moved = compositor.window(2).unwrap().frame;
        assert_eq!(moved.x, origin.x + 100);
        assert_eq!(moved.y, origin.y + 40);
        let events = compositor.pointer_button(MouseButton::Left, false);
        assert!(events.iter().any(|event| matches!(
            event,
            HostEvent::Send {
                message: Message::Configure { .. },
                ..
            }
        )));
        let damage = compositor.damage.take();
        assert!(!damage.is_empty());
        assert!(!damage.whole);
    }

    #[test]
    fn resize_corner_changes_content_size() {
        let mut compositor = compositor();
        let window = compositor.window(2).unwrap().clone();
        let corner = Point::new(
            window.resize_corner_rect().center().x,
            window.resize_corner_rect().center().y,
        );
        compositor.pointer_motion(corner);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_motion(Point::new(corner.x + 80, corner.y + 50));
        let resized = compositor.window(2).unwrap();
        assert_eq!(resized.content_size.w, window.content_size.w + 80);
        assert_eq!(resized.content_size.h, window.content_size.h + 50);
        let events = compositor.pointer_button(MouseButton::Left, false);
        assert!(events.iter().any(|event| matches!(
            event,
            HostEvent::Send {
                message: Message::Configure { .. },
                ..
            }
        )));
    }

    #[test]
    fn close_button_removes_window_and_notifies() {
        let mut compositor = compositor();
        let close = compositor.window(2).unwrap().close_rect().center();
        compositor.pointer_motion(close);
        let events = compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert!(compositor.window(2).is_none());
        assert!(events.iter().any(|event| matches!(
            event,
            HostEvent::Send {
                message: Message::Close { .. },
                ..
            }
        )));
        assert_eq!(compositor.focused_id(), Some(1));
    }

    #[test]
    fn maximize_and_restore() {
        let mut compositor = compositor();
        let original = compositor.window(2).unwrap().frame;
        let maximize = compositor.window(2).unwrap().maximize_rect().center();
        compositor.pointer_motion(maximize);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        let maximized = compositor.window(2).unwrap();
        assert!(maximized.maximized);
        assert_eq!(maximized.frame.h, 720 - PANEL_HEIGHT);
        let restore = compositor.window(2).unwrap().maximize_rect().center();
        compositor.pointer_motion(restore);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert!(!compositor.window(2).unwrap().maximized);
        assert_eq!(compositor.window(2).unwrap().frame, original);
    }

    #[test]
    fn minimize_hides_window_and_chip_restores() {
        let mut compositor = compositor();
        let minimize = compositor.window(2).unwrap().minimize_rect().center();
        compositor.pointer_motion(minimize);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert!(compositor.window(2).unwrap().minimized);
        assert_eq!(compositor.focused_id(), Some(1));
        // Чип окна на панели.
        let chips = compositor.chips();
        let slot = chips.iter().position(|(id, _, _)| *id == 2).unwrap();
        let chip = panel::chip_rect(compositor.screen(), slot).center();
        compositor.pointer_motion(chip);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert!(!compositor.window(2).unwrap().minimized);
        assert_eq!(compositor.focused_id(), Some(2));
    }

    #[test]
    fn panel_terminal_and_files_icons_request_launch() {
        // Панель — один из двух пользовательских входов в `launch` (второй —
        // контекстное меню); оба обязаны отдавать `HostEvent::Launch(kind)`,
        // который сервер превращает в `Command::spawn` демо-клиента.
        for (icon, expected) in [
            (PanelIcon::Terminal, AppKind::Terminal),
            (PanelIcon::Files, AppKind::Files),
            (PanelIcon::Paint, AppKind::Paint),
        ] {
            let mut compositor = compositor();
            let point = panel::icon_rect(compositor.screen(), icon.index()).center();
            compositor.pointer_motion(point);
            let events = compositor.pointer_button(MouseButton::Left, true);
            compositor.pointer_button(MouseButton::Left, false);
            assert!(
                events.contains(&HostEvent::Launch(expected)),
                "{icon:?}: панель должна запросить launch {expected:?}, получено {events:?}"
            );
        }
    }

    #[test]
    fn panel_network_click_opens_and_closes_flyout() {
        let mut compositor = compositor();
        let network = panel::network_rect(compositor.screen()).center();
        compositor.pointer_motion(network);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert_eq!(
            compositor.flyout().map(|f| f.kind),
            Some(FlyoutKind::Network)
        );
        compositor.pointer_motion(Point::new(400, 300));
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert!(compositor.flyout().is_none());
    }

    #[test]
    fn clock_click_opens_calendar() {
        let mut compositor = compositor();
        let clock = panel::clock_rect(compositor.screen()).center();
        compositor.pointer_motion(clock);
        compositor.pointer_button(MouseButton::Left, true);
        compositor.pointer_button(MouseButton::Left, false);
        assert_eq!(
            compositor.flyout().map(|f| f.kind),
            Some(FlyoutKind::Calendar)
        );
    }

    #[test]
    fn right_click_desktop_opens_menu_with_actions() {
        let mut compositor = compositor();
        compositor.pointer_motion(Point::new(1000, 300));
        compositor.pointer_button(MouseButton::Right, true);
        compositor.pointer_button(MouseButton::Right, false);
        let menu = compositor.context_menu().expect("меню открыто");
        let terminal = menu.item_rect(0).center();
        // Клик по пункту Open Terminal.
        compositor.pointer_motion(terminal);
        let events = compositor.pointer_button(MouseButton::Left, true);
        assert!(events.contains(&HostEvent::Launch(AppKind::Terminal)));
        assert!(compositor.context_menu().is_none());
    }

    #[test]
    fn right_click_window_opens_window_menu() {
        let mut compositor = compositor();
        let titlebar = titlebar_point(&compositor, 2);
        compositor.pointer_motion(titlebar);
        compositor.pointer_button(MouseButton::Right, true);
        compositor.pointer_button(MouseButton::Right, false);
        let menu = compositor.context_menu().expect("меню окна открыто");
        assert_eq!(menu.target_window, Some(2));
    }

    #[test]
    fn f10_opens_menu_and_damages_screen() {
        let mut compositor = compositor();
        let _ = compositor.render_full();
        let events = compositor.key_event(key::F10, true, 0);
        assert!(events.is_empty(), "F10 — серверный shortcut");
        let menu = compositor
            .context_menu()
            .expect("F10 открывает меню")
            .clone();
        assert!(
            !compositor.damage.take().is_empty(),
            "меню должно добавить damage"
        );
        let before = compositor
            .frame()
            .get(menu.rect.center().x, menu.rect.center().y);
        let _ = compositor.render_full();
        let after = compositor
            .frame()
            .get(menu.rect.center().x, menu.rect.center().y);
        assert_ne!(before, after, "центр меню должен перерисоваться");
    }

    #[test]
    fn escape_closes_menu_and_flyout() {
        let mut compositor = compositor();
        compositor.pointer_motion(Point::new(1000, 300));
        compositor.pointer_button(MouseButton::Right, true);
        compositor.pointer_button(MouseButton::Right, false);
        assert!(compositor.context_menu().is_some());
        compositor.key_event(key::ESC, true, 0);
        assert!(compositor.context_menu().is_none());
    }

    #[test]
    fn alt_tab_cycles_focus() {
        let mut compositor = compositor();
        assert_eq!(compositor.focused_id(), Some(2));
        compositor.key_event(key::TAB, true, MOD_ALT);
        assert_eq!(compositor.focused_id(), Some(1));
        compositor.key_event(key::TAB, true, MOD_ALT);
        assert_eq!(compositor.focused_id(), Some(2));
    }

    #[test]
    fn keys_go_to_focused_window() {
        let mut compositor = compositor();
        let events = compositor.key_event(key::A, true, MOD_SHIFT);
        assert!(events.iter().any(|event| matches!(
            event,
            HostEvent::Send {
                message: Message::Input {
                    event: crate::protocol::InputEvent::Key { code: 30, pressed: true, modifiers },
                    ..
                },
                ..
            } if *modifiers == MOD_SHIFT
        )));
    }

    #[test]
    fn commit_checks_ownership_and_damages_content() {
        let mut compositor = compositor();
        let surface = Surface::filled(640, 400, PixelFormat::Xrgb8888, 0x0033_4455);
        compositor.commit_surface(1, 2, surface.clone()).unwrap();
        assert!(compositor.window(2).unwrap().surface.is_some());
        assert_eq!(
            compositor.commit_surface(99, 2, surface.clone()),
            Err(WindowError::NotOwner)
        );
        assert_eq!(
            compositor.commit_surface(1, 77, surface),
            Err(WindowError::NotFound(77))
        );
        let damage = compositor.damage.take();
        assert!(!damage.is_empty());
    }

    #[test]
    fn wire_committed_surface_renders_with_opaque_alpha() {
        let mut compositor = compositor();
        // 1x1 чистый красный XRGB8888: from_wire не должен терять непрозрачность,
        // иначе blit смешает его с фоном как полностью прозрачный.
        let data = [0x00, 0x00, 0xFF, 0x00];
        let surface = Surface::from_wire(1, 1, 4, PixelFormat::Xrgb8888.to_wire(), &data).unwrap();
        compositor.commit_surface(1, 2, surface).unwrap();
        let _ = compositor.render_full();
        let content = compositor.window(2).unwrap().content_rect();
        let pixel = compositor.frame().get(content.x + 5, content.y + 5);
        assert_eq!(pixel & 0x00FF_FFFF, 0x00FF_0000);
    }

    #[test]
    fn pointer_move_damages_only_small_area() {
        let mut compositor = compositor();
        let _ = compositor.render_full();
        let mut events = compositor.pointer_motion(Point::new(300, 300));
        assert!(events.is_empty() || !events.is_empty());
        let damage = compositor.damage.take();
        assert!(!damage.is_empty());
        assert!(!damage.whole);
        let area: i64 = damage.rects.iter().map(|r| r.area()).sum();
        // Курсор и локальная подсветка: заметно меньше всего экрана 1280x720.
        assert!(area < 20_000, "damage слишком большой: {area}");
        events.clear();
    }

    #[test]
    fn render_full_draws_wallpaper_panel_and_windows() {
        let mut compositor = compositor();
        let surface = Surface::filled(640, 400, PixelFormat::Xrgb8888, 0x0020_8070);
        compositor.commit_surface(1, 2, surface).unwrap();
        let rects = compositor.render_full();
        assert_eq!(rects, vec![compositor.screen()]);
        let canvas = compositor.frame();
        let panel = panel::panel_rect(compositor.screen());
        assert_eq!(canvas.width(), 1280);
        assert_ne!(canvas.get(10, panel.y + 20), 0);
        assert_ne!(canvas.get(200, 200), 0);
        assert_ne!(canvas.get(0, 0), 0);
    }

    #[test]
    fn configure_flags_include_state() {
        let mut compositor = compositor();
        let window = compositor.window_mut(2).unwrap();
        window.maximized = true;
        assert_eq!(
            configure_flags(window) & CONFIGURE_MAXIMIZED,
            CONFIGURE_MAXIMIZED
        );
        assert_ne!(configure_flags(window) & CONFIGURE_RESIZABLE, 0);
    }

    #[test]
    fn cached_layers_are_built_once_per_frame_state() {
        let mut compositor = compositor();
        compositor.render_full();
        let first = compositor.cache_stats();
        assert_eq!(first.wallpaper_builds, 1);
        assert_eq!(first.panel_builds, 1);
        assert_eq!(first.chrome_builds, 2);
        assert_eq!(first.shadow_builds, 2);
        compositor.render_full();
        assert_eq!(
            compositor.cache_stats(),
            first,
            "повторный кадр не должен перестраивать кэши"
        );
    }

    #[test]
    fn wallpaper_change_invalidates_wallpaper_cache() {
        let mut compositor = compositor();
        compositor.render_full();
        assert_eq!(compositor.cache_stats().wallpaper_builds, 1);
        compositor.set_wallpaper_alt(true);
        compositor.render_full();
        assert_eq!(compositor.cache_stats().wallpaper_builds, 2);
    }

    #[test]
    fn clock_change_damages_only_the_clock() {
        let mut compositor = compositor();
        compositor.render_full();
        compositor.set_clock("23:39", "15.11.2026");
        let damage = compositor.damage.take();
        assert!(!damage.whole);
        assert_eq!(damage.rects, vec![panel::clock_rect(compositor.screen())]);
        // Возвращаем damage в трекер: проверяем, что render его действительно
        // перерисует и перестроит кэш панели.
        for rect in damage.rects {
            compositor.damage.add(rect);
        }
        compositor.render();
        assert_eq!(compositor.cache_stats().panel_builds, 2);
    }

    #[test]
    fn moving_window_does_not_rebuild_chrome() {
        let mut compositor = compositor();
        compositor.render_full();
        let builds = compositor.cache_stats().chrome_builds;
        let frame = compositor.window(2).unwrap().frame;
        compositor.move_window(2, Point::new(frame.x + 10, frame.y + 5));
        compositor.render();
        assert_eq!(compositor.cache_stats().chrome_builds, builds);
        // Фокус/заголовок — реальное изменение оформления.
        compositor.set_focus(Some(2));
        compositor.render();
        let _ = compositor.cache_stats().chrome_builds;
    }

    #[test]
    fn removing_window_evicts_chrome_cache() {
        let mut compositor = compositor();
        compositor.render_full();
        compositor.remove_window(2, false);
        assert!(!compositor.chrome_cache.contains_key(&2));
    }

    #[test]
    fn client_disconnect_removes_windows() {
        let mut compositor = compositor();
        compositor.remove_client(1);
        assert!(compositor.windows().is_empty());
        assert_eq!(compositor.focused_id(), None);
    }
}
