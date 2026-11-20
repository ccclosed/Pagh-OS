//! Files — приложение файлового менеджера для рабочего стола Pagh.
//!
//! Приложение владеет состоянием окна (текущая папка, выделение, режим
//! просмотра, меню, диалоги) и рисует **всю** поверхность окна само, включая
//! заголовок `v`/`^`/`x`: композитор только композитит готовый кадр. События
//! приходят из [`pagh_ui::event`] (от композитора или из тестов/демо), а
//! действия, требующие внешнего мира (терминал, запуск файла), возвращаются
//! вызывающему как [`Command`].
//!
//! Данные — через трейт [`Vfs`]: на Pagh это будущая реализация поверх реальных
//! файловых вызовов, в тестах и демо — [`crate::fake::FakeVfs`].

use pagh_ui::canvas::Canvas;
use pagh_ui::chrome;
use pagh_ui::color::Color;
use pagh_ui::event::{Event, Key, Modifiers, MouseButton};
use pagh_ui::font::{self, TextAlign};
use pagh_ui::geom::{Insets, Point, Rect, Size};
use pagh_ui::icon::{self, Icon};
use pagh_ui::layout;
use pagh_ui::menu::{self, Menu, MenuLayout};
use pagh_ui::theme::Theme;
use pagh_ui::widgets::{Button, ButtonState, Grid, GridItem, List, ListItem, TextField};

use crate::format;
use crate::icons::{self, FluentIcon};
use crate::menu_model::{self, action, MenuOptions, SortMode, ViewMode};
use crate::vfs::{self, DirEntry, EntryKind, Stat, Vfs};

/// Ширина поля поиска в панели инструментов.
const SEARCH_WIDTH: i32 = 220;
/// Размер кнопки-иконки в панели инструментов.
const TOOLBAR_BUTTON: i32 = 28;
/// Высота строки подробного списка.
const LIST_ROW: i32 = 26;
/// Высота шапки подробного списка.
const LIST_HEADER: i32 = 24;
/// Шаг прокрутки колесом.
const WHEEL_STEP: i32 = 48;
/// Прямоугольник иконки в ячейке сетки — как `ICON_WIDTH`/`ICON_HEIGHT`
/// в `pagh_ui::widgets` (значения там приватные). По нему стирается
/// векторный глиф перед отрисовкой готового растра.
const GRID_ICON_WIDTH: i32 = 40;
const GRID_ICON_HEIGHT: i32 = 32;
const GRID_ICON_TOP: i32 = 8;

/// Действие, которое приложение просит выполнить снаружи.
///
/// Оформление окна (заголовок, `v`/`^`/`x`, панель) принадлежит композитору,
/// поэтому свернуть/развернуть/закрыть окно клиент не может — он лишь
/// завершается по `close` от сервера. [`Command::LaunchTerminal`] отправляется
/// композитору сообщением PDP/1 `launch` (type 13).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    /// Завершить приложение (окно закрыто композитором).
    Quit,
    /// Открыть терминал в каталоге (PDP/1 `launch` с app = "terminal").
    LaunchTerminal { path: String },
    /// Открыть файл/каталог внешним приложением.
    OpenPath { path: String },
}

/// Кнопки панели инструментов (для наведения).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToolbarButton {
    Back,
    Forward,
    Search,
    Kebab,
}

/// Откуда берётся путь пункта боковой панели.
///
/// Это не косметика: от вида зависит, что приложение имеет право делать с
/// путём при старте. Каталоги пользователя (`xdg-user-dirs`) создаются, а
/// устройство — только проверяется на существование; выдумывать точку
/// монтирования нельзя.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceKind {
    /// Домашний каталог (`$HOME`).
    Home,
    /// Каталог пользователя: `Desktop`, `Documents`, … — канонические имена
    /// XDG. Отсутствующий создаётся при старте (так же поступает
    /// `xdg-user-dirs-update`), иначе клик вёл бы в несуществующий путь.
    UserDir,
    /// Корзина: реальный каталог `$HOME/.local/share/Trash/files`. Создаётся
    /// при старте, как и каталоги пользователя. Files этой версии удаляет
    /// безвозвратно, поэтому корзина может быть пуста — но это настоящий
    /// каталог, а не выдуманный листинг.
    Trash,
    /// Сеть: виртуальная точка входа. Сетевого ФС-бэкенда у Files нет, и
    /// придумывать содержимое нельзя, поэтому пункт открывается с честным
    /// «нет сетевых ресурсов», а не с ошибкой или фальшивым списком.
    Network,
    /// Устройство: реальная точка монтирования VFS. Пункт показывается,
    /// только если путь разрешается.
    Device,
}

/// Пункт боковой панели.
#[derive(Clone, Debug)]
pub struct Place {
    pub id: u32,
    pub label: String,
    /// Готовая иконка Fluent; векторный `pagh_ui::Icon` из неё выводится
    /// только чтобы список зарезервировал место под глиф (текст пункта
    /// сдвигается на ширину иконки), а рисуется растр.
    pub glyph: FluentIcon,
    pub path: String,
    pub kind: PlaceKind,
}

/// Что находится в фокусе клавиатуры.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    None,
    Path,
    Search,
}

/// Счётчики для строки состояния.
#[derive(Clone, Copy, Debug, Default)]
struct Status {
    dirs: u64,
    files: u64,
    bytes: u64,
}

/// Открытое контекстное меню.
struct OpenMenu {
    model: Menu,
    origin: Point,
    hovered: Option<usize>,
    submenu_open: Option<usize>,
    submenu_hovered: Option<usize>,
}

/// Открытый диалог.
enum Dialog {
    Properties(Stat),
}

/// Что создаём из подменю «Создать».
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CreateKind {
    Folder,
    TextFile,
    EmptyFile,
    HtmlFile,
    UrlLink,
    Symlink,
    AppLink,
}

/// Буфер обмена Files (путь скопированного элемента).
#[derive(Clone, Debug)]
struct Clipboard {
    path: String,
}

/// Геометрия клиентской области Files: панель инструментов, боковая панель,
/// содержимое и строка состояния (заголовок окна рисует композитор).
#[derive(Clone, Debug)]
pub struct AppLayout {
    pub toolbar: Rect,
    pub back: Rect,
    pub forward: Rect,
    pub path_field: Rect,
    pub search_button: Rect,
    pub kebab: Rect,
    pub search_field: Option<Rect>,
    pub sidebar: Rect,
    pub sidebar_list: pagh_ui::widgets::ListLayout,
    pub content: Rect,
    pub grid_area: Rect,
    pub status: Rect,
}

/// Приложение Files.
pub struct FilesApp {
    vfs: Box<dyn Vfs>,
    pub theme: Theme,
    size: Size,
    cwd: String,
    all_entries: Vec<DirEntry>,
    entries: Vec<DirEntry>,
    selected: Option<usize>,
    hovered: Option<usize>,
    places: Vec<Place>,
    sidebar: List,
    sidebar_selected: Option<u32>,
    sidebar_hover: Option<u32>,
    toolbar_hover: Option<ToolbarButton>,
    path: TextField,
    search: TextField,
    search_visible: bool,
    sort: SortMode,
    view: ViewMode,
    history: Vec<String>,
    history_pos: usize,
    scroll: i32,
    list_hover: Option<usize>,
    menu: Option<OpenMenu>,
    dialog: Option<Dialog>,
    clipboard: Option<Clipboard>,
    status: Status,
    message: Option<String>,
    error: Option<String>,
    /// Пояснение к пустой области содержимого вместо «Папка пуста» — сейчас
    /// это только виртуальная «Сеть» без бэкенда.
    notice: Option<String>,
}

impl FilesApp {
    /// Создать приложение, открыв домашний каталог.
    pub fn new(vfs: Box<dyn Vfs>, size: Size) -> FilesApp {
        let home = vfs::normalize(vfs.home());
        let places = default_places(&home);
        let sidebar = build_sidebar(&places);
        let mut path = TextField::new("Путь");
        path.set_text(display_path(&home, &home));
        let search = TextField::new("Поиск по папке").with_icon(Icon::Search);
        let mut app = FilesApp {
            vfs,
            theme: Theme::pagh_dark(),
            size,
            cwd: home.clone(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected: None,
            hovered: None,
            places,
            sidebar,
            sidebar_selected: Some(1),
            sidebar_hover: None,
            toolbar_hover: None,
            path,
            search,
            search_visible: false,
            sort: SortMode::Name,
            view: ViewMode::Grid,
            history: vec![home.clone()],
            history_pos: 0,
            scroll: 0,
            list_hover: None,
            menu: None,
            dialog: None,
            clipboard: None,
            status: Status::default(),
            message: None,
            error: None,
            notice: None,
        };
        // Устройства — только те точки монтирования, что разрешаются в этой
        // ФС; каталоги пользователя доводим до существующих (см. PlaceKind).
        app.prune_missing_devices();
        app.ensure_user_dirs();
        app.sidebar = build_sidebar(&app.places);
        app.reload();
        app
    }

    // ───────────────────────── доступ к состоянию ─────────────────────────

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn home(&self) -> String {
        self.vfs.home().to_string()
    }

    /// Видимые (не скрытые, прошедшие поиск) элементы в порядке отображения.
    pub fn entries(&self) -> &[DirEntry] {
        &self.entries
    }

    pub fn selected(&self) -> Option<&DirEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn view(&self) -> ViewMode {
        self.view
    }

    pub fn sort(&self) -> SortMode {
        self.sort
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Пункты боковой панели.
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    /// Человеческое имя текущего каталога (метка точки входа, если совпала).
    pub fn location_label(&self) -> String {
        if let Some(place) = self
            .places
            .iter()
            .find(|place| vfs::normalize(&place.path) == self.cwd)
        {
            return place.label.clone();
        }
        vfs::display_name(&self.cwd, self.vfs.home())
    }

    /// Заголовок окна: `Домашняя папка — Files`.
    pub fn title(&self) -> String {
        format!("{} — Files", self.location_label())
    }

    /// Текст строки состояния (как в Dolphin).
    pub fn status_text(&self) -> String {
        format::status_text(self.status.dirs, self.status.files, self.status.bytes)
    }

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub fn search_visible(&self) -> bool {
        self.search_visible
    }

    /// Модель открытого меню (для тестов и отладки).
    pub fn menu_model(&self) -> Option<&Menu> {
        self.menu.as_ref().map(|open| &open.model)
    }

    /// Модель открытого подменю.
    pub fn submenu_model(&self) -> Option<&Menu> {
        let open = self.menu.as_ref()?;
        let index = open.submenu_open?;
        open.model
            .entries
            .get(index)
            .and_then(|entry| entry.submenu_of())
    }

    /// Текущее смещение прокрутки содержимого.
    pub fn scroll_offset(&self) -> i32 {
        self.scroll
    }

    /// Раскладка меню (для тестов и демо-сцен).
    pub fn menu_layouts(&self) -> Option<(MenuLayout, Option<MenuLayout>)> {
        let open = self.menu.as_ref()?;
        let root = menu::layout(&self.theme, &open.model, open.origin);
        let submenu = open.submenu_open.and_then(|index| {
            open.model
                .entries
                .get(index)
                .and_then(|entry| entry.submenu_of())
                .map(|submenu| {
                    let origin = self.submenu_origin(&root, submenu, index);
                    menu::layout(&self.theme, submenu, origin)
                })
        });
        Some((root, submenu))
    }

    /// Точка подменю: справа от пункта, а если не помещается — слева от меню.
    fn submenu_origin(&self, root: &MenuLayout, submenu: &Menu, index: usize) -> Point {
        let bounds = self.window_rect();
        let size = menu::measure(&self.theme, submenu);
        let preferred = root
            .submenu_origin(index)
            .unwrap_or_else(|| Point::new(root.rect.right(), root.rect.y));
        let mut x = preferred.x;
        if x + size.w > bounds.right() - 4 {
            x = root.rect.x - size.w + 6;
        }
        let mut y = preferred.y;
        if y + size.h > bounds.bottom() - 4 {
            y = (bounds.bottom() - 4 - size.h).max(bounds.y + 4);
        }
        Point::new(x.max(bounds.x + 4), y)
    }

    /// Диалог свойств: прямоугольник панели и кнопки (для тестов).
    pub fn dialog_rect(&self) -> Rect {
        Rect::new(0, 0, 460, 320).centered_in(self.window_rect())
    }

    // ───────────────────────── навигация ─────────────────────────

    /// Открыть каталог; путь нормализуется.
    pub fn navigate_to(&mut self, path: &str) {
        let normalized = vfs::normalize(path);
        if normalized == self.cwd {
            self.reload();
            return;
        }
        self.cwd = normalized;
        self.selected = None;
        self.hovered = None;
        self.scroll = 0;
        self.search.set_text("");
        self.path.set_text(display_path(&self.cwd, self.vfs.home()));
        self.history.truncate(self.history_pos + 1);
        self.history.push(self.cwd.clone());
        self.history_pos = self.history.len() - 1;
        self.message = None;
        self.reload();
    }

    /// Открыть элемент: папку — навигацией, файл — командой внешнему миру.
    fn open_entries(&mut self, index: usize) -> Vec<Command> {
        let Some(entry) = self.entries.get(index).cloned() else {
            return Vec::new();
        };
        if entry.is_dir() {
            self.navigate_to(&vfs::join(&self.cwd, &entry.name));
            Vec::new()
        } else {
            vec![Command::OpenPath {
                path: vfs::join(&self.cwd, &entry.name),
            }]
        }
    }

    pub fn go_back(&mut self) {
        if self.history_pos > 0 {
            self.history_pos -= 1;
            self.cwd = self.history[self.history_pos].clone();
            self.selected = None;
            self.scroll = 0;
            self.path.set_text(display_path(&self.cwd, self.vfs.home()));
            self.reload();
        }
    }

    pub fn go_forward(&mut self) {
        if self.history_pos + 1 < self.history.len() {
            self.history_pos += 1;
            self.cwd = self.history[self.history_pos].clone();
            self.selected = None;
            self.scroll = 0;
            self.path.set_text(display_path(&self.cwd, self.vfs.home()));
            self.reload();
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.history_pos > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_pos + 1 < self.history.len()
    }

    /// Оставить в «Устройствах» только реально смонтированные точки.
    ///
    /// Таблица монтирования ядра (`/proc/mounts`) приложению недоступна на
    /// хосте, а путь в VFS — общий признак для гостя и хоста: устройство,
    /// которое не разрешается, не показываем вместо того, чтобы вести клик в
    /// несуществующий каталог.
    fn prune_missing_devices(&mut self) {
        let mut kept = Vec::with_capacity(self.places.len());
        for place in self.places.drain(..) {
            if place.kind == PlaceKind::Device && !self.vfs.exists(&place.path) {
                continue;
            }
            kept.push(place);
        }
        self.places = kept;
    }

    /// Создать отсутствующие каталоги пользователя (`Desktop`, `Documents`, …).
    ///
    /// Это поведение `xdg-user-dirs-update`: клик по пункту боковой панели
    /// должен открывать каталог, а не показывать ошибку. Создаём только пути
    /// из [`PlaceKind::UserDir`] (внутри `$HOME`); устройства и корень не
    /// трогаем. Если ФС только для чтения, `mkdir` не удастся — пункт останется
    /// и честно покажет ошибку открытия.
    fn ensure_user_dirs(&mut self) {
        let paths: Vec<String> = self
            .places
            .iter()
            .filter(|place| matches!(place.kind, PlaceKind::UserDir | PlaceKind::Trash))
            .map(|place| place.path.clone())
            .collect();
        for path in paths {
            self.ensure_dir_all(&path);
        }
    }

    /// `mkdir -p`: создать каждый отсутствующий компонент пути.
    fn ensure_dir_all(&mut self, path: &str) {
        let normalized = vfs::normalize(path);
        let mut prefix = String::new();
        for part in normalized.split('/').filter(|part| !part.is_empty()) {
            prefix.push('/');
            prefix.push_str(part);
            if self.vfs.exists(&prefix) {
                continue;
            }
            // Ошибку не разыгрываем: пункт остаётся, а клик по нему покажет
            // настоящее «Не удалось открыть папку: …» из read_dir.
            let _ = self.vfs.mkdir(&prefix);
        }
    }

    /// Перечитать каталог и пересчитать статус.
    pub fn reload(&mut self) {
        self.sidebar_selected = self
            .places
            .iter()
            .find(|place| vfs::normalize(&place.path) == self.cwd)
            .map(|place| place.id);
        // «Сеть» — виртуальная точка: читать её из VFS значило бы показать
        // «Не удалось открыть папку». Сетевого бэкенда нет, поэтому содержимое
        // пустое и об этом сказано прямо; выдуманных ресурсов здесь нет.
        let virtual_network = self.places.iter().any(|place| {
            place.kind == PlaceKind::Network && vfs::normalize(&place.path) == self.cwd
        });
        if virtual_network {
            self.all_entries.clear();
            self.error = None;
            self.notice = Some("Нет сетевых ресурсов".to_string());
            self.apply_filter_sort();
            return;
        }
        self.notice = None;
        match self.vfs.read_dir(&self.cwd) {
            Ok(entries) => {
                self.all_entries = entries;
                self.error = None;
            }
            Err(error) => {
                self.all_entries.clear();
                self.error = Some(error.to_string());
            }
        }
        self.apply_filter_sort();
    }

    fn apply_filter_sort(&mut self) {
        let query = self.search.text().to_lowercase();
        let mut entries: Vec<DirEntry> = self
            .all_entries
            .iter()
            .filter(|entry| !vfs::is_hidden(&entry.name))
            .filter(|entry| query.is_empty() || entry.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let sort = self.sort;
        entries.sort_by(|a, b| {
            // Папки всегда выше файлов, как в Dolphin.
            b.is_dir()
                .cmp(&a.is_dir())
                .then_with(|| match sort {
                    SortMode::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortMode::Size => b.size.cmp(&a.size),
                    SortMode::Modified => b.modified.cmp(&a.modified),
                })
                .then_with(|| a.name.cmp(&b.name))
        });
        self.entries = entries;
        let dirs = self.entries.iter().filter(|entry| entry.is_dir()).count() as u64;
        let files = self.entries.iter().filter(|entry| !entry.is_dir()).count() as u64;
        let bytes = self
            .entries
            .iter()
            .filter(|entry| !entry.is_dir())
            .map(|entry| entry.size)
            .sum();
        self.status = Status { dirs, files, bytes };
        if let Some(index) = self.selected {
            if index >= self.entries.len() {
                self.selected = None;
            }
        }
    }

    /// Задать режим просмотра (используется меню и тестами).
    pub fn set_view(&mut self, view: ViewMode) {
        self.view = view;
        self.scroll = 0;
    }

    /// Открыть контекстное меню в точке.
    pub fn open_context_menu(&mut self, pos: Point) {
        let options = MenuOptions {
            clipboard: self.clipboard.is_some(),
            view: self.view,
            sort: self.sort,
        };
        let model = menu_model::context_menu(options);
        let origin = clamp_menu_origin(&self.theme, &model, pos, self.window_rect());
        self.menu = Some(OpenMenu {
            model,
            origin,
            hovered: None,
            submenu_open: None,
            submenu_hovered: None,
        });
    }

    /// Открыть меню «кебаба» под кнопкой.
    pub fn open_toolbar_menu(&mut self) {
        let options = MenuOptions {
            clipboard: self.clipboard.is_some(),
            view: self.view,
            sort: self.sort,
        };
        let model = menu_model::toolbar_menu(options);
        let kebab = self.layout().kebab;
        let origin = Point::new(kebab.x - 6, kebab.bottom() + 4);
        let origin = clamp_menu_origin(&self.theme, &model, origin, self.window_rect());
        self.menu = Some(OpenMenu {
            model,
            origin,
            hovered: None,
            submenu_open: None,
            submenu_hovered: None,
        });
    }

    fn close_menu(&mut self) {
        self.menu = None;
    }

    fn open_properties(&mut self) {
        let stat = match self.selected() {
            Some(entry) => {
                let path = vfs::join(&self.cwd, &entry.name);
                self.vfs.stat(&path)
            }
            None => self.vfs.stat(&self.cwd),
        };
        match stat {
            Ok(stat) => self.dialog = Some(Dialog::Properties(stat)),
            Err(error) => self.message = Some(format!("Свойства недоступны: {error}")),
        }
    }

    // ───────────────────────── операции ─────────────────────────

    fn create(&mut self, kind: CreateKind) {
        let selected_path = self
            .selected()
            .map(|entry| vfs::join(&self.cwd, &entry.name))
            .unwrap_or_else(|| self.cwd.clone());
        let (base, extension) = match kind {
            CreateKind::Folder => ("Новая папка", None),
            CreateKind::TextFile => ("Новый текстовый файл", Some("txt")),
            CreateKind::EmptyFile => ("Новый файл", None),
            CreateKind::HtmlFile => ("Новый файл", Some("html")),
            CreateKind::UrlLink => ("Новая ссылка", Some("url")),
            CreateKind::Symlink => ("Новая ссылка", None),
            CreateKind::AppLink => ("Новое приложение", Some("desktop")),
        };
        let name = vfs::unique_name(&self.all_entries, base, extension);
        let path = vfs::join(&self.cwd, &name);
        let result = match kind {
            CreateKind::Folder => self.vfs.mkdir(&path),
            CreateKind::TextFile | CreateKind::EmptyFile => self.vfs.create_file(&path),
            CreateKind::HtmlFile => self.vfs.write_file(&path, HTML_TEMPLATE.as_bytes()),
            CreateKind::UrlLink => self.vfs.write_file(&path, URL_TEMPLATE.as_bytes()),
            CreateKind::AppLink => self.vfs.write_file(&path, DESKTOP_TEMPLATE.as_bytes()),
            CreateKind::Symlink => self.vfs.symlink(&selected_path, &path),
        };
        match result {
            Ok(()) => {
                self.message = Some(format!("Создано: {name}"));
                self.reload();
                if let Some(index) = self.entries.iter().position(|entry| entry.name == name) {
                    self.selected = Some(index);
                }
            }
            Err(error) => self.message = Some(format!("Не удалось создать: {error}")),
        }
    }

    fn copy_selected(&mut self) {
        if let Some(entry) = self.selected().cloned() {
            let path = vfs::join(&self.cwd, &entry.name);
            self.clipboard = Some(Clipboard { path });
            self.message = Some(format!("Скопировано: {}", entry.name));
        }
    }

    fn paste(&mut self) {
        let Some(clipboard) = self.clipboard.clone() else {
            self.message = Some("Буфер обмена пуст".to_string());
            return;
        };
        let name = vfs::file_name(&clipboard.path).to_string();
        let target_name = vfs::copy_name(&self.all_entries, &name);
        let target = vfs::join(&self.cwd, &target_name);
        match self.vfs.copy(&clipboard.path, &target) {
            Ok(()) => {
                self.message = Some(format!("Вставлено: {target_name}"));
                self.reload();
                if let Some(index) = self
                    .entries
                    .iter()
                    .position(|entry| entry.name == target_name)
                {
                    self.selected = Some(index);
                }
            }
            Err(error) => self.message = Some(format!("Не удалось вставить: {error}")),
        }
    }

    fn delete_selected(&mut self) {
        let Some(entry) = self.selected().cloned() else {
            return;
        };
        let path = vfs::join(&self.cwd, &entry.name);
        match self.vfs.unlink(&path) {
            Ok(()) => {
                self.message = Some(format!("Удалено: {}", entry.name));
                self.selected = None;
                self.reload();
            }
            Err(error) => self.message = Some(format!("Не удалось удалить: {error}")),
        }
    }

    fn perform_action(&mut self, id: u32) -> Vec<Command> {
        match id {
            action::CREATE_FOLDER => self.create(CreateKind::Folder),
            action::CREATE_TEXT_FILE => self.create(CreateKind::TextFile),
            action::CREATE_EMPTY_FILE => self.create(CreateKind::EmptyFile),
            action::CREATE_HTML_FILE => self.create(CreateKind::HtmlFile),
            action::CREATE_URL_LINK => self.create(CreateKind::UrlLink),
            action::CREATE_SYMLINK => self.create(CreateKind::Symlink),
            action::CREATE_APP_LINK => self.create(CreateKind::AppLink),
            action::PASTE => self.paste(),
            action::SORT_NAME => {
                self.sort = SortMode::Name;
                self.apply_filter_sort();
            }
            action::SORT_SIZE => {
                self.sort = SortMode::Size;
                self.apply_filter_sort();
            }
            action::SORT_MODIFIED => {
                self.sort = SortMode::Modified;
                self.apply_filter_sort();
            }
            action::VIEW_GRID => self.set_view(ViewMode::Grid),
            action::VIEW_LIST => self.set_view(ViewMode::List),
            action::OPEN_TERMINAL => {
                return vec![Command::LaunchTerminal {
                    path: self.cwd.clone(),
                }]
            }
            action::OPEN_IN_APP => {
                return vec![Command::OpenPath {
                    path: self.cwd.clone(),
                }]
            }
            action::PROPERTIES => self.open_properties(),
            action::REFRESH => {
                self.reload();
                self.message = Some("Папка обновлена".to_string());
            }
            _ => self.message = Some("Пункт пока не поддерживается".to_string()),
        }
        Vec::new()
    }

    // ───────────────────────── геометрия ─────────────────────────

    fn window_rect(&self) -> Rect {
        Rect::new(0, 0, self.size.w, self.size.h)
    }

    /// Рассчитать раскладку клиентской области.
    pub fn layout(&self) -> AppLayout {
        let client = self.window_rect();
        let (toolbar, rest) = client.split_top(self.theme.metrics.toolbar_height);
        let (rest, status) = rest.split_bottom(self.theme.metrics.status_height);
        let (sidebar, content) = rest.split_left(self.theme.metrics.sidebar_width);

        let y = toolbar.y + (toolbar.h - TOOLBAR_BUTTON) / 2;
        let back = Rect::new(toolbar.x + 8, y, TOOLBAR_BUTTON, TOOLBAR_BUTTON);
        let forward = Rect::new(back.right() + 4, y, TOOLBAR_BUTTON, TOOLBAR_BUTTON);
        let kebab = Rect::new(
            toolbar.right() - 8 - TOOLBAR_BUTTON,
            y,
            TOOLBAR_BUTTON,
            TOOLBAR_BUTTON,
        );
        let search_button = Rect::new(
            kebab.x - 6 - TOOLBAR_BUTTON,
            y,
            TOOLBAR_BUTTON,
            TOOLBAR_BUTTON,
        );
        let search_field = if self.search_visible {
            Some(Rect::new(
                search_button.x - 8 - SEARCH_WIDTH,
                y,
                SEARCH_WIDTH,
                TOOLBAR_BUTTON,
            ))
        } else {
            None
        };
        let path_right = search_field
            .map(|rect| rect.x - 8)
            .unwrap_or(search_button.x - 8);
        let path_left = forward.right() + 8;
        let path_field = Rect::new(
            path_left,
            y,
            (path_right - path_left).max(80),
            TOOLBAR_BUTTON,
        );

        let sidebar_area = sidebar.inset_by(Insets::new(8, 8, 10, 8));
        let sidebar_list = self.sidebar.layout(sidebar_area, &self.theme);

        let content_area = content.inset_by(Insets::new(14, 12, 12, 10));
        AppLayout {
            toolbar,
            back,
            forward,
            path_field,
            search_button,
            kebab,
            search_field,
            sidebar,
            sidebar_list,
            content,
            grid_area: content_area,
            status,
        }
    }

    fn grid_cols(&self, area: Rect) -> i32 {
        layout::columns_for(area.w, 118, 8)
    }

    fn grid_cell(&self, area: Rect) -> Size {
        let cols = self.grid_cols(area);
        let gap = 8;
        let width = ((area.w - gap * (cols - 1)) / cols).max(96);
        Size::new(width, 92)
    }

    fn grid_widget(&self, area: Rect) -> Grid {
        let mut grid = Grid::new(self.grid_cols(area));
        grid.cell = self.grid_cell(area);
        grid.gap = Size::new(8, 8);
        grid.selected = self.selected;
        grid.hovered = self.hovered;
        for entry in &self.entries {
            grid.push(GridItem {
                label: entry.name.clone(),
                icon: legacy_icon(entry_icon(entry)),
                is_dir: entry.is_dir(),
            });
        }
        grid
    }

    fn scrolled_area(&self, area: Rect) -> Rect {
        area.offset(0, -self.scroll)
    }

    fn content_height(&self, area: Rect) -> i32 {
        match self.view {
            ViewMode::Grid => {
                let grid = self.grid_widget(area);
                grid.content_height(area)
            }
            ViewMode::List => LIST_HEADER + self.entries.len() as i32 * LIST_ROW,
        }
    }

    fn max_scroll(&self, area: Rect) -> i32 {
        (self.content_height(area) - area.h).max(0)
    }

    fn list_columns(&self, area: Rect) -> [Rect; 4] {
        let size_w = 96;
        let kind_w = 108;
        let modified_w = 140;
        let name_w = (area.w - 30 - size_w - kind_w - modified_w).max(120);
        let y = area.y;
        let h = area.h;
        [
            Rect::new(area.x + 30, y, name_w, h),
            Rect::new(area.x + 30 + name_w, y, size_w, h),
            Rect::new(area.x + 30 + name_w + size_w, y, kind_w, h),
            Rect::new(area.x + 30 + name_w + size_w + kind_w, y, modified_w, h),
        ]
    }

    fn list_row_at(&self, area: Rect, pos: Point) -> Option<usize> {
        if !area.contains(pos) {
            return None;
        }
        let rows_area = Rect::new(
            area.x,
            area.y + LIST_HEADER - self.scroll,
            area.w,
            self.entries.len() as i32 * LIST_ROW,
        );
        if !rows_area.contains(pos) {
            return None;
        }
        let index = (pos.y - rows_area.y) / LIST_ROW;
        if index >= 0 && (index as usize) < self.entries.len() {
            Some(index as usize)
        } else {
            None
        }
    }

    /// Прямоугольник элемента по индексу (для тестов и демо-сцен).
    pub fn item_rect(&self, index: usize) -> Option<Rect> {
        let layout = self.layout();
        match self.view {
            ViewMode::Grid => self
                .grid_widget(layout.grid_area)
                .layout(self.scrolled_area(layout.grid_area))
                .into_iter()
                .nth(index),
            ViewMode::List => {
                if index >= self.entries.len() {
                    return None;
                }
                Some(Rect::new(
                    layout.grid_area.x,
                    layout.grid_area.y + LIST_HEADER + index as i32 * LIST_ROW - self.scroll,
                    layout.grid_area.w,
                    LIST_ROW,
                ))
            }
        }
    }

    /// Выделить элемент по индексу.
    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index;
        if index.is_some() {
            self.ensure_selected_visible();
        }
    }

    /// Сменить размер поверхности (событие `configure` от композитора).
    pub fn resize(&mut self, size: Size) {
        self.size = size;
        let max = self.max_scroll(self.layout().grid_area);
        self.scroll = self.scroll.clamp(0, max);
    }

    /// Открыть поле поиска и задать запрос (демо/тесты).
    pub fn show_search(&mut self, query: &str) {
        self.search_visible = true;
        self.search.set_text(query);
        self.set_focus(Focus::Search);
        self.apply_filter_sort();
    }

    /// Текст запроса поиска.
    pub fn search_query(&self) -> String {
        self.search.text()
    }

    fn grid_index_at(&self, area: Rect, pos: Point) -> Option<usize> {
        let grid = self.grid_widget(area);
        grid.index_at(self.scrolled_area(area), pos)
    }

    // ───────────────────────── отрисовка ─────────────────────────

    /// Нарисовать клиентскую область Files.
    ///
    /// Композитор рисует вокруг неё заголовок окна, рамку и панель; клиент не
    /// знает про `v`/`^`/`x` и не рисует их.
    pub fn draw(&self, canvas: &mut Canvas) {
        let layout = self.layout();
        canvas.fill(self.window_rect(), self.theme.palette.window);
        self.draw_toolbar(canvas, &layout);
        self.draw_sidebar(canvas, &layout);
        self.draw_content(canvas, &layout);
        self.draw_statusbar(canvas, &layout);
        if self.menu.is_some() {
            self.draw_menu(canvas);
        }
        if let Some(Dialog::Properties(stat)) = &self.dialog {
            self.draw_properties(canvas, stat);
        }
    }

    fn draw_toolbar(&self, canvas: &mut Canvas, layout: &AppLayout) {
        let palette = &self.theme.palette;
        canvas.fill(
            Rect::new(
                layout.toolbar.x,
                layout.toolbar.y,
                layout.toolbar.w,
                layout.toolbar.h,
            ),
            palette.toolbar,
        );
        canvas.hline(
            layout.toolbar.bottom() - 1,
            layout.toolbar.x,
            layout.toolbar.right(),
            palette.separator,
        );

        // Назад / вперёд.
        for (button, rect, enabled) in [
            (ToolbarButton::Back, layout.back, self.can_go_back()),
            (
                ToolbarButton::Forward,
                layout.forward,
                self.can_go_forward(),
            ),
        ] {
            let hovered = self.toolbar_hover == Some(button);
            if hovered && enabled {
                canvas.fill_rounded(rect, self.theme.metrics.radius_control, palette.selection);
            }
            let color = if enabled {
                if hovered {
                    palette.text
                } else {
                    palette.text_muted
                }
            } else {
                palette.text_disabled
            };
            let icon = match button {
                ToolbarButton::Back => Icon::ChevronLeft,
                _ => Icon::ChevronRight,
            };
            icon::draw(
                canvas,
                icon,
                Rect::new(rect.x + 7, rect.y + 7, 14, 14),
                color,
                palette,
            );
        }

        // Поле пути.
        let path_rect = layout.path_field;
        canvas.fill_rounded(path_rect, self.theme.metrics.radius_control, palette.field);
        canvas.stroke_rounded(
            path_rect,
            self.theme.metrics.radius_control,
            1,
            if self.focused() == Focus::Path {
                palette.field_border_focused
            } else {
                palette.field_border
            },
        );
        let path_text = if self.focused() == Focus::Path {
            self.path.text()
        } else {
            self.location_label()
        };
        let shown = font::ellipsize(&path_text, path_rect.w - 20);
        font::draw(
            canvas,
            path_rect.x + 10,
            font::baseline_for(path_rect),
            &shown,
            palette.text,
        );
        if self.focused() == Focus::Path {
            let caret = path_rect.x + 10 + font::measure(&shown);
            canvas.fill(
                Rect::new(caret, path_rect.y + 7, 1, path_rect.h - 14),
                palette.accent,
            );
        }

        // Поиск.
        if let Some(search_rect) = layout.search_field {
            canvas.fill_rounded(
                search_rect,
                self.theme.metrics.radius_control,
                palette.field,
            );
            canvas.stroke_rounded(
                search_rect,
                self.theme.metrics.radius_control,
                1,
                if self.focused() == Focus::Search {
                    palette.field_border_focused
                } else {
                    palette.field_border
                },
            );
            icon::draw(
                canvas,
                Icon::Search,
                Rect::new(search_rect.x + 7, search_rect.y + 7, 14, 14),
                palette.text_muted,
                palette,
            );
            let text = if self.search.is_empty() {
                self.search_placeholder()
            } else {
                self.search.text()
            };
            font::draw(
                canvas,
                search_rect.x + 28,
                font::baseline_for(search_rect),
                &font::ellipsize(&text, search_rect.w - 40),
                if self.search.is_empty() {
                    palette.placeholder
                } else {
                    palette.text
                },
            );
            if self.focused() == Focus::Search {
                let caret = search_rect.x + 28 + font::measure(&self.search.text());
                canvas.fill(
                    Rect::new(caret, search_rect.y + 7, 1, search_rect.h - 14),
                    palette.accent,
                );
            }
        }

        // Кнопки поиска и меню.
        for (button, rect, icon_kind) in [
            (ToolbarButton::Search, layout.search_button, Icon::Search),
            (ToolbarButton::Kebab, layout.kebab, Icon::MenuDots),
        ] {
            let hovered = self.toolbar_hover == Some(button);
            if hovered {
                canvas.fill_rounded(rect, self.theme.metrics.radius_control, palette.selection);
            }
            icon::draw(
                canvas,
                icon_kind,
                Rect::new(rect.x + 7, rect.y + 7, 14, 14),
                if hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
                palette,
            );
        }
    }

    fn search_placeholder(&self) -> String {
        "Поиск по папке".to_string()
    }

    fn focused(&self) -> Focus {
        if self.path.is_focused() {
            Focus::Path
        } else if self.search.is_focused() {
            Focus::Search
        } else {
            Focus::None
        }
    }

    fn draw_sidebar(&self, canvas: &mut Canvas, layout: &AppLayout) {
        let palette = &self.theme.palette;
        canvas.fill(layout.sidebar, palette.sidebar);
        canvas.vline(
            layout.sidebar.right() - 1,
            layout.sidebar.y,
            layout.sidebar.bottom(),
            palette.sidebar_border,
        );
        let mut list = self.sidebar.clone();
        list.selected = self.sidebar_selected;
        list.hovered = self.sidebar_hover;
        let area = layout.sidebar.inset_by(Insets::new(8, 8, 10, 8));
        let list_layout = list.draw(canvas, &self.theme, area);
        // Поверх векторных глифов списка — готовые иконки Fluent. Пункт
        // списка всё равно несёт `Icon` того же смысла: он резервирует место
        // под глиф и служит запасным вариантом, если PNG не декодировался.
        let icon_size = self.theme.metrics.icon_size;
        for (id, rect) in &list_layout.items {
            let Some(place) = self.places.iter().find(|place| place.id == *id) else {
                continue;
            };
            let icon_rect = Rect::new(
                rect.x + 8,
                rect.y + (rect.h - icon_size) / 2,
                icon_size,
                icon_size,
            );
            let tint = if self.sidebar_selected == Some(*id) {
                palette.sidebar_text_selected
            } else {
                palette.sidebar_text
            };
            draw_glyph(canvas, place.glyph, icon_rect, tint, true);
        }
    }

    fn draw_content(&self, canvas: &mut Canvas, layout: &AppLayout) {
        let palette = &self.theme.palette;
        canvas.fill(layout.content, palette.window);
        canvas.with_clip(layout.content, |canvas| {
            let area = layout.grid_area;
            if let Some(error) = &self.error {
                let text = format!("Не удалось открыть папку: {error}");
                font::draw_aligned(
                    canvas,
                    Rect::new(area.x, area.y + 40, area.w, 20),
                    &text,
                    palette.text_muted,
                    TextAlign::Center,
                );
                return;
            }
            if self.entries.is_empty() {
                let text = if let Some(notice) = &self.notice {
                    notice.clone()
                } else if self.search.is_empty() {
                    "Папка пуста".to_string()
                } else {
                    format!("Ничего не найдено по запросу «{}»", self.search.text())
                };
                font::draw_aligned(
                    canvas,
                    Rect::new(area.x, area.y + 40, area.w, 20),
                    &text,
                    palette.text_muted,
                    TextAlign::Center,
                );
                return;
            }
            match self.view {
                ViewMode::Grid => {
                    let grid = self.grid_widget(area);
                    let cells = grid.draw(canvas, &self.theme, self.scrolled_area(area));
                    // Векторные глифы ячеек заменяем готовыми иконками.
                    for (index, cell) in cells.iter().enumerate() {
                        let Some(entry) = self.entries.get(index) else {
                            continue;
                        };
                        let icon_rect = Rect::new(
                            cell.x + (cell.w - GRID_ICON_WIDTH) / 2,
                            cell.y + GRID_ICON_TOP,
                            GRID_ICON_WIDTH,
                            GRID_ICON_HEIGHT,
                        );
                        draw_entry_icon(canvas, entry, icon_rect, palette, true);
                    }
                }
                ViewMode::List => self.draw_list(canvas, area),
            }
            let content_height = self.content_height(area);
            if content_height > area.h {
                let track = Rect::new(area.right() + 2, area.y, 6, area.h);
                let thumb_h =
                    ((track.h as i64 * area.h as i64) / content_height as i64).max(28) as i32;
                let max_scroll = (content_height - area.h).max(1);
                let offset =
                    ((track.h - thumb_h) as i64 * self.scroll as i64 / max_scroll as i64) as i32;
                canvas.fill_rounded(
                    Rect::new(track.x, track.y + offset, track.w, thumb_h),
                    3,
                    palette.scrollbar,
                );
            }
        });
    }

    fn draw_list(&self, canvas: &mut Canvas, area: Rect) {
        let palette = &self.theme.palette;
        let columns = self.list_columns(area);
        let header = Rect::new(area.x, area.y, area.w, LIST_HEADER);
        canvas.fill(header, palette.toolbar);
        canvas.hline(
            header.bottom() - 1,
            header.x,
            header.right(),
            palette.separator,
        );
        let sort_column = match self.sort {
            SortMode::Name => 0,
            SortMode::Size => 1,
            SortMode::Modified => 3,
        };
        for (index, (rect, title, align)) in [
            (columns[0], "Имя", TextAlign::Left),
            (columns[1], "Размер", TextAlign::Right),
            (columns[2], "Тип", TextAlign::Left),
            (columns[3], "Изменён", TextAlign::Left),
        ]
        .into_iter()
        .enumerate()
        {
            let inner = if align == TextAlign::Right {
                Rect::new(rect.x, header.y, rect.w - 10, header.h)
            } else {
                Rect::new(rect.x + 10, header.y, rect.w - 10, header.h)
            };
            let width = font::draw_aligned(canvas, inner, title, palette.section_text, align);
            if sort_column == index {
                let arrow_x = match align {
                    TextAlign::Right => inner.right() - width - 12,
                    _ => inner.x + width + 4,
                };
                icon::draw(
                    canvas,
                    Icon::ChevronUp,
                    Rect::new(arrow_x, header.y + (header.h - 8) / 2, 8, 8),
                    palette.section_text,
                    palette,
                );
            }
        }

        let rows_area = Rect::new(
            area.x,
            area.y + LIST_HEADER - self.scroll,
            area.w,
            self.entries.len() as i32 * LIST_ROW,
        );
        for (index, entry) in self.entries.iter().enumerate() {
            let row = Rect::new(
                area.x,
                rows_area.y + index as i32 * LIST_ROW,
                area.w,
                LIST_ROW,
            );
            if row.bottom() < area.y + LIST_HEADER || row.y > area.bottom() {
                continue;
            }
            if self.selected == Some(index) {
                canvas.fill_rounded(
                    row.inset(2),
                    self.theme.metrics.radius_control,
                    palette.selection,
                );
            } else if self.list_hover == Some(index) {
                canvas.fill_rounded(
                    row.inset(2),
                    self.theme.metrics.radius_control,
                    Color::rgba(255, 255, 255, 8),
                );
            }
            let icon_rect = Rect::new(row.x + 8, row.y + (LIST_ROW - 14) / 2, 14, 14);
            draw_entry_icon(canvas, entry, icon_rect, palette, false);
            let name = font::ellipsize(&entry.name, columns[0].w - 34);
            font::draw(
                canvas,
                columns[0].x,
                font::baseline_for(row),
                &name,
                if entry.is_dir() {
                    palette.text
                } else {
                    palette.text_muted
                },
            );
            font::draw_aligned(
                canvas,
                Rect::new(columns[1].x, row.y, columns[1].w - 10, row.h),
                &if entry.is_dir() {
                    "—".to_string()
                } else {
                    format::format_size(entry.size)
                },
                palette.text_muted,
                TextAlign::Right,
            );
            font::draw(
                canvas,
                columns[2].x + 10,
                font::baseline_for(row),
                entry.kind.label(),
                palette.text_muted,
            );
            font::draw(
                canvas,
                columns[3].x + 10,
                font::baseline_for(row),
                &format::format_date(entry.modified),
                palette.text_muted,
            );
        }
    }

    fn draw_statusbar(&self, canvas: &mut Canvas, layout: &AppLayout) {
        let left = self.message.clone().unwrap_or_else(|| self.status_text());
        let right = self.location_label();
        chrome::draw_statusbar(canvas, &self.theme, layout.status, &left, &right);
    }

    fn draw_menu(&self, canvas: &mut Canvas) {
        let Some(open) = &self.menu else {
            return;
        };
        let root = menu::draw(canvas, &self.theme, &open.model, open.origin, open.hovered);
        if let Some(index) = open.submenu_open {
            if let Some(submenu) = open
                .model
                .entries
                .get(index)
                .and_then(|entry| entry.submenu_of())
            {
                let origin = self.submenu_origin(&root, submenu, index);
                menu::draw(canvas, &self.theme, submenu, origin, open.submenu_hovered);
            }
        }
    }

    fn draw_properties(&self, canvas: &mut Canvas, stat: &Stat) {
        let palette = &self.theme.palette;
        let theme = &self.theme;
        let window = self.window_rect();
        canvas.fill(window, Color::rgba(0, 0, 0, 120));
        let rect = self.dialog_rect();
        canvas.soft_shadow(rect, 8, 20, palette.shadow);
        canvas.fill_rounded(rect, theme.metrics.radius_window, palette.window);
        canvas.stroke_rounded(rect, theme.metrics.radius_window, 1, palette.window_border);
        let (titlebar, body) = rect.split_top(38);
        canvas.fill_rounded(titlebar, theme.metrics.radius_window, palette.titlebar);
        canvas.fill(
            Rect::new(
                titlebar.x,
                titlebar.bottom() - theme.metrics.radius_window,
                titlebar.w,
                theme.metrics.radius_window,
            ),
            palette.titlebar,
        );
        font::draw(
            canvas,
            titlebar.x + 14,
            font::baseline_for(titlebar),
            "Свойства",
            palette.title_text,
        );
        let close = Rect::new(
            titlebar.right() - 8 - 26,
            titlebar.y + (titlebar.h - 26) / 2,
            26,
            26,
        );
        icon::draw(
            canvas,
            Icon::Close,
            Rect::new(close.x + 6, close.y + 6, 14, 14),
            palette.title_text,
            palette,
        );

        let mut y = body.y + 18;
        let rows: Vec<(&str, String)> = vec![
            ("Имя", stat.name.clone()),
            ("Тип", stat.kind.label().to_string()),
            (
                "Размер",
                if stat.kind.is_dir() {
                    "—".to_string()
                } else {
                    format::format_size(stat.size)
                },
            ),
            ("Изменён", format::format_date(stat.modified)),
            ("Права", format::format_mode(stat.mode, stat.kind.is_dir())),
            ("Путь", stat.path.clone()),
        ];
        for (label, value) in rows {
            font::draw(
                canvas,
                body.x + 18,
                y + font::ascent(),
                label,
                palette.text_muted,
            );
            let value_rect = Rect::new(body.x + 110, y - 2, body.w - 128, font::line_height() + 4);
            font::draw(
                canvas,
                value_rect.x,
                font::ascent() + y,
                &font::ellipsize(&value, value_rect.w),
                palette.text,
            );
            y += 26;
        }
        if let Some(target) = &stat.target {
            font::draw(
                canvas,
                body.x + 18,
                y + font::ascent(),
                "Цель",
                palette.text_muted,
            );
            font::draw(
                canvas,
                body.x + 110,
                y + font::ascent(),
                &font::ellipsize(target, body.w - 128),
                palette.text,
            );
        }

        let button = Button::primary("Закрыть");
        let button_rect = Rect::new(
            rect.right() - 18 - button.min_size(theme).w,
            rect.bottom() - 18 - 32,
            button.min_size(theme).w,
            32,
        );
        button.draw(canvas, theme, button_rect, ButtonState::default());
    }

    // ───────────────────────── события ─────────────────────────

    /// Обработать одно событие; вернуть команды наружу.
    pub fn handle_event(&mut self, event: &Event) -> Vec<Command> {
        match event {
            Event::MouseMove { pos } => {
                self.on_mouse_move(*pos);
                Vec::new()
            }
            Event::MouseDown { pos, button } => self.on_mouse_down(*pos, *button),
            Event::MouseUp { .. } => Vec::new(),
            Event::DoubleClick { pos } => self.on_double_click(*pos),
            Event::Wheel { pos, delta } => {
                let layout = self.layout();
                if layout.content.contains(*pos) {
                    let max = self.max_scroll(layout.grid_area);
                    self.scroll = (self.scroll - delta * WHEEL_STEP).clamp(0, max);
                }
                Vec::new()
            }
            Event::Key {
                key,
                pressed,
                modifiers,
            } => {
                if *pressed {
                    self.on_key(*key, *modifiers)
                } else {
                    Vec::new()
                }
            }
            Event::Text { text } => {
                self.on_text(text);
                Vec::new()
            }
            Event::Resize { size } => {
                self.size = *size;
                Vec::new()
            }
            Event::Focus { .. } => Vec::new(),
            Event::CloseRequest => vec![Command::Quit],
        }
    }

    fn on_mouse_move(&mut self, pos: Point) {
        let layout = self.layout();
        if self.menu.is_some() {
            self.update_menu_hover(pos);
            return;
        }
        if self.dialog.is_some() {
            return;
        }
        self.toolbar_hover = [
            (ToolbarButton::Back, layout.back),
            (ToolbarButton::Forward, layout.forward),
            (ToolbarButton::Search, layout.search_button),
            (ToolbarButton::Kebab, layout.kebab),
        ]
        .into_iter()
        .find(|(_, rect)| rect.contains(pos))
        .map(|(button, _)| button);
        self.sidebar_hover = layout.sidebar_list.item_at(pos);
        self.hovered = match self.view {
            ViewMode::Grid => self.grid_index_at(layout.grid_area, pos),
            ViewMode::List => self.list_row_at(layout.grid_area, pos),
        };
        self.list_hover = if self.view == ViewMode::List {
            self.list_row_at(layout.grid_area, pos)
        } else {
            None
        };
    }

    fn update_menu_hover(&mut self, pos: Point) {
        let Some((root, submenu)) = self.menu_layouts() else {
            return;
        };
        let submenu_hovered = submenu
            .as_ref()
            .and_then(|layout| layout.hit(pos))
            .filter(|_| {
                submenu
                    .as_ref()
                    .map(|l| l.rect.contains(pos))
                    .unwrap_or(false)
            });
        let root_hit = root.hit(pos);
        if let Some(open) = self.menu.as_mut() {
            if submenu_hovered.is_some() {
                open.submenu_hovered = submenu_hovered;
                return;
            }
            open.submenu_hovered = None;
            if let Some(index) = root_hit {
                let is_submenu = open
                    .model
                    .entries
                    .get(index)
                    .map(|entry| entry.submenu_of().is_some())
                    .unwrap_or(false);
                let enabled = open
                    .model
                    .entries
                    .get(index)
                    .map(|entry| entry.is_enabled())
                    .unwrap_or(false);
                if enabled {
                    open.hovered = Some(index);
                    open.submenu_open = if is_submenu { Some(index) } else { None };
                } else {
                    open.hovered = Some(index);
                    open.submenu_open = None;
                }
            } else {
                open.hovered = None;
                open.submenu_open = None;
            }
        }
    }

    fn on_mouse_down(&mut self, pos: Point, button: MouseButton) -> Vec<Command> {
        // Диалог модален.
        if let Some(Dialog::Properties(_)) = self.dialog {
            let rect = self.dialog_rect();
            let close_rect = Rect::new(rect.right() - 34, rect.y + 6, 26, 26);
            let button_rect = Rect::new(rect.right() - 120, rect.bottom() - 50, 100, 32);
            if button == MouseButton::Left
                && (close_rect.contains(pos) || button_rect.contains(pos) || !rect.contains(pos))
            {
                self.dialog = None;
            }
            return Vec::new();
        }

        if self.menu.is_some() {
            return self.on_menu_click(pos, button);
        }

        if button == MouseButton::Right {
            let layout = self.layout();
            if layout.grid_area.contains(pos) {
                match self.view {
                    ViewMode::Grid => {
                        if let Some(index) = self.grid_index_at(layout.grid_area, pos) {
                            self.selected = Some(index);
                        }
                    }
                    ViewMode::List => {
                        if let Some(index) = self.list_row_at(layout.grid_area, pos) {
                            self.selected = Some(index);
                        }
                    }
                }
                self.open_context_menu(pos);
            } else if layout.sidebar.contains(pos) {
                self.open_context_menu(Point::new(layout.sidebar.right() - 8, pos.y));
            } else if layout.kebab.contains(pos) {
                self.open_toolbar_menu();
            }
            return Vec::new();
        }

        let layout = self.layout();
        if layout.kebab.contains(pos) {
            self.open_toolbar_menu();
            return Vec::new();
        }
        if layout.search_button.contains(pos) {
            self.search_visible = !self.search_visible;
            if self.search_visible {
                self.set_focus(Focus::Search);
            } else {
                self.set_focus(Focus::None);
                self.search.set_text("");
                self.apply_filter_sort();
            }
            return Vec::new();
        }
        if layout.back.contains(pos) {
            self.go_back();
            return Vec::new();
        }
        if layout.forward.contains(pos) {
            self.go_forward();
            return Vec::new();
        }
        if layout.path_field.contains(pos) {
            self.set_focus(Focus::Path);
            return Vec::new();
        }
        if layout
            .search_field
            .map(|rect| rect.contains(pos))
            .unwrap_or(false)
        {
            self.set_focus(Focus::Search);
            return Vec::new();
        }
        if let Some(id) = layout.sidebar_list.item_at(pos) {
            if let Some(place) = self.places.iter().find(|place| place.id == id) {
                let path = place.path.clone();
                self.navigate_to(&path);
            }
            return Vec::new();
        }
        if layout.grid_area.contains(pos) {
            self.set_focus(Focus::None);
            match self.view {
                ViewMode::Grid => {
                    self.selected = self.grid_index_at(layout.grid_area, pos);
                }
                ViewMode::List => {
                    self.selected = self.list_row_at(layout.grid_area, pos);
                }
            }
            return Vec::new();
        }
        self.set_focus(Focus::None);
        Vec::new()
    }

    fn on_menu_click(&mut self, pos: Point, button: MouseButton) -> Vec<Command> {
        if button == MouseButton::Right {
            self.close_menu();
            return Vec::new();
        }
        let Some((root, submenu)) = self.menu_layouts() else {
            return Vec::new();
        };
        // Клик по подменю.
        if let Some(submenu_layout) = &submenu {
            if submenu_layout.rect.contains(pos) {
                if let Some(index) = submenu_layout.hit(pos) {
                    let entry = self
                        .menu
                        .as_ref()
                        .and_then(|open| open.submenu_open)
                        .and_then(|parent| {
                            self.menu
                                .as_ref()?
                                .model
                                .entries
                                .get(parent)?
                                .submenu_of()?
                                .entries
                                .get(index)
                        });
                    if let Some(entry) = entry {
                        if let Some(id) = entry.id() {
                            if entry.is_enabled() {
                                self.close_menu();
                                return self.perform_action(id);
                            }
                        }
                    }
                }
                return Vec::new();
            }
        }
        if root.rect.contains(pos) {
            if let Some(index) = root.hit(pos) {
                let entry = self
                    .menu
                    .as_ref()
                    .and_then(|open| open.model.entries.get(index));
                if let Some(entry) = entry {
                    if entry.submenu_of().is_some() {
                        if entry.is_enabled() {
                            if let Some(open) = self.menu.as_mut() {
                                open.submenu_open = Some(index);
                                open.submenu_hovered = None;
                            }
                        }
                        return Vec::new();
                    }
                    if let Some(id) = entry.id() {
                        if entry.is_enabled() {
                            self.close_menu();
                            return self.perform_action(id);
                        }
                    }
                }
                return Vec::new();
            }
        }
        self.close_menu();
        Vec::new()
    }

    fn on_double_click(&mut self, pos: Point) -> Vec<Command> {
        if self.menu.is_some() || self.dialog.is_some() {
            return Vec::new();
        }
        let layout = self.layout();
        if !layout.grid_area.contains(pos) {
            return Vec::new();
        }
        let index = match self.view {
            ViewMode::Grid => self.grid_index_at(layout.grid_area, pos),
            ViewMode::List => self.list_row_at(layout.grid_area, pos),
        };
        match index {
            Some(index) => {
                self.selected = Some(index);
                self.open_entries(index)
            }
            None => Vec::new(),
        }
    }

    fn set_focus(&mut self, focus: Focus) {
        self.path.set_focused(focus == Focus::Path);
        self.search.set_focused(focus == Focus::Search);
        if focus == Focus::Path {
            self.path.set_text(self.cwd.clone());
        }
    }

    fn on_text(&mut self, text: &str) {
        let event = Event::Text {
            text: text.to_string(),
        };
        match self.focused() {
            Focus::Path => {
                self.path.handle_event(&event);
            }
            Focus::Search => {
                if !self.search_visible {
                    self.search_visible = true;
                    self.search.set_focused(true);
                }
                if self.search.handle_event(&event) {
                    self.apply_filter_sort();
                    self.selected = None;
                }
            }
            Focus::None => {}
        }
    }

    fn on_key(&mut self, key: Key, modifiers: Modifiers) -> Vec<Command> {
        // Диалог.
        if self.dialog.is_some() {
            if key == Key::Escape || key == Key::Enter {
                self.dialog = None;
            }
            return Vec::new();
        }
        // Меню.
        if let Some(open) = self.menu.as_mut() {
            match key {
                Key::Escape => {
                    self.menu = None;
                }
                Key::Down | Key::Up => {
                    let delta = if key == Key::Down { 1 } else { -1 };
                    let enabled: Vec<usize> = open
                        .model
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| entry.is_enabled())
                        .map(|(index, _)| index)
                        .collect();
                    if !enabled.is_empty() {
                        let next = match open.hovered {
                            Some(current) => {
                                let position =
                                    enabled.iter().position(|i| *i == current).unwrap_or(0);
                                let next_position = (position as i32 + delta)
                                    .clamp(0, enabled.len() as i32 - 1)
                                    as usize;
                                enabled[next_position]
                            }
                            None => enabled[0],
                        };
                        open.hovered = Some(next);
                        open.submenu_open = None;
                        open.submenu_hovered = None;
                    }
                }
                Key::Right | Key::Enter => {
                    if let Some(index) = open.hovered {
                        if open
                            .model
                            .entries
                            .get(index)
                            .map(|entry| entry.submenu_of().is_some() && entry.is_enabled())
                            .unwrap_or(false)
                        {
                            open.submenu_open = Some(index);
                            open.submenu_hovered = None;
                        } else if key == Key::Enter {
                            if let Some(entry) = open.model.entries.get(index) {
                                if let Some(id) = entry.id() {
                                    if entry.is_enabled() {
                                        self.menu = None;
                                        return self.perform_action(id);
                                    }
                                }
                            }
                        }
                    }
                }
                Key::Left => {
                    open.submenu_open = None;
                    open.submenu_hovered = None;
                }
                _ => {}
            }
            return Vec::new();
        }

        // Поле пути/поиска.
        match self.focused() {
            Focus::Path => match key {
                Key::Enter => {
                    let text = self.path.text();
                    let target = if text.starts_with('/') {
                        text
                    } else {
                        vfs::join(&self.cwd, &text)
                    };
                    if self
                        .vfs
                        .stat(&target)
                        .map(|stat| stat.kind.is_dir())
                        .unwrap_or(false)
                    {
                        self.set_focus(Focus::None);
                        self.navigate_to(&target);
                    } else {
                        self.message = Some(format!("Это не папка: {target}"));
                    }
                    return Vec::new();
                }
                Key::Escape => {
                    self.set_focus(Focus::None);
                    self.path.set_text(display_path(&self.cwd, self.vfs.home()));
                    return Vec::new();
                }
                _ => {
                    self.path.handle_event(&Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                    });
                    return Vec::new();
                }
            },
            Focus::Search => match key {
                Key::Escape => {
                    self.set_focus(Focus::None);
                    self.search_visible = false;
                    self.search.set_text("");
                    self.apply_filter_sort();
                    return Vec::new();
                }
                Key::Enter => return Vec::new(),
                _ => {
                    if self.search.handle_event(&Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                    }) {
                        self.apply_filter_sort();
                        self.selected = None;
                    }
                    return Vec::new();
                }
            },
            Focus::None => {}
        }

        // Глобальные сочетания.
        if modifiers.alt && modifiers.shift && key == Key::F4 {
            return vec![Command::LaunchTerminal {
                path: self.cwd.clone(),
            }];
        }
        if modifiers.alt && key == Key::Enter {
            self.open_properties();
            return Vec::new();
        }
        if modifiers.ctrl && key == Key::Char('v') {
            self.paste();
            return Vec::new();
        }
        if modifiers.ctrl && key == Key::Char('c') {
            self.copy_selected();
            return Vec::new();
        }
        match key {
            Key::Escape => {
                self.search_visible = false;
                self.search.set_text("");
                self.selected = None;
                self.apply_filter_sort();
            }
            Key::Delete => self.delete_selected(),
            Key::F4 => {
                self.open_toolbar_menu();
            }
            Key::Enter => {
                if let Some(index) = self.selected {
                    return self.open_entries(index);
                }
            }
            Key::Down | Key::Up | Key::Left | Key::Right | Key::Home | Key::End => {
                self.move_selection(key);
            }
            _ => {}
        }
        Vec::new()
    }

    fn move_selection(&mut self, key: Key) {
        if self.entries.is_empty() {
            return;
        }
        let cols = match self.view {
            ViewMode::Grid => self.grid_cols(self.layout().grid_area).max(1),
            ViewMode::List => 1,
        };
        let last = self.entries.len() as i32 - 1;
        let Some(current) = self.selected.map(|value| value as i32) else {
            // Ничего не выделено: стрелки вперёд/вниз выделяют первый элемент,
            // назад/вверх — последний.
            let index = match key {
                Key::Up | Key::Left | Key::End => last,
                _ => 0,
            };
            self.selected = Some(index.max(0) as usize);
            self.ensure_selected_visible();
            return;
        };
        let next = match key {
            Key::Left => current - 1,
            Key::Right => current + 1,
            Key::Up => current - cols,
            Key::Down => current + cols,
            Key::Home => 0,
            Key::End => last,
            _ => current,
        }
        .clamp(0, last);
        self.selected = Some(next as usize);
        self.ensure_selected_visible();
    }

    fn ensure_selected_visible(&mut self) {
        let layout = self.layout();
        let area = layout.grid_area;
        let Some(index) = self.selected else {
            return;
        };
        let (top, bottom) = match self.view {
            ViewMode::Grid => {
                let grid = self.grid_widget(area);
                match grid.layout(area).get(index) {
                    Some(rect) => (rect.y, rect.bottom()),
                    None => return,
                }
            }
            ViewMode::List => (
                area.y + LIST_HEADER + index as i32 * LIST_ROW,
                area.y + LIST_HEADER + (index as i32 + 1) * LIST_ROW,
            ),
        };
        if top - self.scroll < area.y {
            self.scroll = top - area.y;
        } else if bottom - self.scroll > area.bottom() {
            self.scroll = bottom - area.bottom();
        }
        self.scroll = self.scroll.clamp(0, self.max_scroll(area));
    }
}

fn display_path(path: &str, home: &str) -> String {
    if vfs::normalize(path) == vfs::normalize(home) {
        "Домашняя папка".to_string()
    } else {
        vfs::normalize(path)
    }
}

fn entry_icon(entry: &DirEntry) -> FluentIcon {
    match entry.kind {
        EntryKind::Directory => FluentIcon::Folder,
        // Готового глифа ссылки в наборе нет; ссылка выглядит как документ,
        // тип записи при этом остаётся честным (колонка «Тип»).
        EntryKind::Symlink => FluentIcon::Document,
        EntryKind::Device => FluentIcon::Drive,
        EntryKind::Other | EntryKind::File => file_icon(&entry.name),
    }
}

/// Иконка файла по расширению (изображение/аудио/видео/документ).
fn file_icon(name: &str) -> FluentIcon {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff"
        | "avif" => FluentIcon::Image,
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" | "opus" | "mid" | "midi" => {
            FluentIcon::MusicNote
        }
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "m4v" | "wmv" | "mpg" | "mpeg" => {
            FluentIcon::Video
        }
        _ => FluentIcon::Document,
    }
}

/// Векторный глиф того же смысла — запасной вариант, если PNG не декодировался
/// (и маска для резервирования места в списке).
fn legacy_icon(glyph: FluentIcon) -> Icon {
    match glyph {
        FluentIcon::Folder | FluentIcon::FolderOpen => Icon::Folder,
        FluentIcon::Document => Icon::File,
        FluentIcon::Home => Icon::Home,
        FluentIcon::Desktop => Icon::Desktop,
        FluentIcon::Download => Icon::Downloads,
        FluentIcon::Image => Icon::Pictures,
        FluentIcon::MusicNote => Icon::Music,
        FluentIcon::Video => Icon::Videos,
        FluentIcon::Delete => Icon::Trash,
        FluentIcon::Drive => Icon::Device,
        FluentIcon::Network => Icon::Network,
    }
}

/// Нарисовать значок записи готовым растром.
///
/// `erase` — прямоугольник, где виджет сетки/сайдбара уже нарисовал векторный
/// глиф: его нужно закрыть фоном, иначе полупрозрачные края растра оставят
/// двойной контур. Для подробного списка векторного глифа нет, и стирание не
/// нужно. Фон берётся из самого холста рядом с иконкой, поэтому выделение и
/// наведение сохраняются без повторения логики виджета.
fn draw_entry_icon(
    canvas: &mut Canvas,
    entry: &DirEntry,
    rect: Rect,
    palette: &pagh_ui::theme::Palette,
    erase_vector: bool,
) {
    let glyph = entry_icon(entry);
    let tint = if entry.is_dir() {
        palette.accent
    } else {
        palette.text_muted
    };
    draw_glyph(canvas, glyph, rect, tint, erase_vector);
}

/// Общий путь отрисовки готовой иконки: центр `rect`, тинт, опционально
/// стирание векторного глифа.
fn draw_glyph(canvas: &mut Canvas, glyph: FluentIcon, rect: Rect, tint: Color, erase_vector: bool) {
    let size = icons::best_size(rect);
    let Some(raster) = icons::raster(glyph, size) else {
        return;
    };
    if erase_vector {
        // Расширяем зону стирания: растр 16 px чуть больше 14-пиксельного
        // прямоугольника списка и должен закрыть векторный глиф целиком.
        let erase = Rect::new(rect.x - 2, rect.y - 2, rect.w + 4, rect.h + 4);
        let backdrop = canvas.pixel(Point::new(erase.x - 4, erase.center().y));
        canvas.fill(erase, backdrop);
    }
    let dst = Rect::new(
        rect.x + (rect.w - raster.width as i32) / 2,
        rect.y + (rect.h - raster.height as i32) / 2,
        raster.width as i32,
        raster.height as i32,
    );
    icons::blit_tinted(canvas, &raster, dst, tint, 255);
}

/// Поместить меню так, чтобы оно не выходило за окно.
fn clamp_menu_origin(theme: &Theme, menu: &Menu, origin: Point, bounds: Rect) -> Point {
    let size = menu::measure(theme, menu);
    let mut x = origin.x;
    let mut y = origin.y;
    if x + size.w > bounds.right() - 4 {
        x = (bounds.right() - 4 - size.w).max(bounds.x + 4);
    }
    if y + size.h > bounds.bottom() - 4 {
        y = (bounds.bottom() - 4 - size.h).max(bounds.y + 4);
    }
    Point::new(x, y)
}

/// Точки входа боковой панели.
///
/// Каталоги пользователя — канонические имена XDG (`Desktop`, `Documents`,
/// `Downloads`, `Pictures`, `Music`, `Videos`); отсутствующие Files создаёт при
/// старте. «Корзина» — настоящий `~/.local/share/Trash/files` (тоже создаётся):
/// удаление в этой версии безвозвратное (`unlink`), поэтому каталог обычно пуст,
/// но это реальный путь, а не выдумка. «Сеть» — виртуальная точка: сетевого
/// ФС-бэкенда в Files нет, поэтому она открывается пустой с честным
/// «Нет сетевых ресурсов» (см. [`FilesApp::reload`]), без фальшивого листинга.
///
/// Устройства — реальные точки монтирования Pagh: `/` — корень VFS, `/mnt` —
/// смонтированный ext2 (см. `boot.rs`); пункт, которого нет в VFS, отсеивает
/// [`FilesApp::prune_missing_devices`]. Ни `/boot`, ни `/mnt/basic` в дереве
/// ядра не существуют, поэтому их здесь нет.
fn default_places(home: &str) -> Vec<Place> {
    vec![
        Place {
            id: 1,
            label: "Домашняя папка".to_string(),
            glyph: FluentIcon::Home,
            path: home.to_string(),
            kind: PlaceKind::Home,
        },
        Place {
            id: 2,
            label: "Рабочий стол".to_string(),
            glyph: FluentIcon::Desktop,
            path: vfs::join(home, "Desktop"),
            kind: PlaceKind::UserDir,
        },
        Place {
            id: 3,
            label: "Документы".to_string(),
            glyph: FluentIcon::FolderOpen,
            path: vfs::join(home, "Documents"),
            kind: PlaceKind::UserDir,
        },
        Place {
            id: 4,
            label: "Загрузки".to_string(),
            glyph: FluentIcon::Download,
            path: vfs::join(home, "Downloads"),
            kind: PlaceKind::UserDir,
        },
        Place {
            id: 5,
            label: "Изображения".to_string(),
            glyph: FluentIcon::Image,
            path: vfs::join(home, "Pictures"),
            kind: PlaceKind::UserDir,
        },
        Place {
            id: 6,
            label: "Музыка".to_string(),
            glyph: FluentIcon::MusicNote,
            path: vfs::join(home, "Music"),
            kind: PlaceKind::UserDir,
        },
        Place {
            id: 7,
            label: "Видео".to_string(),
            glyph: FluentIcon::Video,
            path: vfs::join(home, "Videos"),
            kind: PlaceKind::UserDir,
        },
        Place {
            id: 8,
            label: "Корзина".to_string(),
            glyph: FluentIcon::Delete,
            path: vfs::join(home, ".local/share/Trash/files"),
            kind: PlaceKind::Trash,
        },
        Place {
            id: 9,
            label: "Сеть".to_string(),
            glyph: FluentIcon::Network,
            path: "/network".to_string(),
            kind: PlaceKind::Network,
        },
        Place {
            id: 10,
            label: "root".to_string(),
            glyph: FluentIcon::Drive,
            path: "/".to_string(),
            kind: PlaceKind::Device,
        },
        Place {
            id: 11,
            label: "Basic data partition".to_string(),
            glyph: FluentIcon::Drive,
            path: "/mnt".to_string(),
            kind: PlaceKind::Device,
        },
    ]
}

fn build_sidebar(places: &[Place]) -> List {
    let entry_items = |ids: &[u32]| -> Vec<ListItem> {
        ids.iter()
            .filter_map(|id| places.iter().find(|place| place.id == *id))
            .map(|place| {
                ListItem::new(place.id, place.label.clone()).with_icon(legacy_icon(place.glyph))
            })
            .collect()
    };
    let mut list = List::new()
        .section("ТОЧКИ ВХОДА", entry_items(&[1, 2, 3, 4, 5, 6, 7, 8]))
        .section("В СЕТИ", entry_items(&[9]))
        .section("УСТРОЙСТВА", entry_items(&[10, 11]));
    // Пустая секция «УСТРОЙСТВА» (диск не смонтирован) не рисуется.
    list.sections.retain(|section| !section.items.is_empty());
    list
}

const HTML_TEMPLATE: &str = "<!DOCTYPE html>\n<html lang=\"ru\">\n<head>\n  <meta charset=\"utf-8\">\n  <title>Новая страница</title>\n</head>\n<body>\n\n</body>\n</html>\n";

const URL_TEMPLATE: &str = "[InternetShortcut]\nURL=https://\n";

const DESKTOP_TEMPLATE: &str =
    "[Desktop Entry]\nType=Application\nName=Новое приложение\nExec=\nIcon=\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeVfs;
    use crate::host_vfs::HostVfs;

    const SIZE: Size = Size::new(1060, 640);
    /// Канонические каталоги пользователя, которые Files обязан создать.
    const USER_DIRS: [&str; 6] = [
        "Desktop",
        "Documents",
        "Downloads",
        "Pictures",
        "Music",
        "Videos",
    ];

    fn click_place(app: &mut FilesApp, label: &str) -> String {
        let place = app
            .places()
            .iter()
            .find(|place| place.label == label)
            .unwrap_or_else(|| panic!("нет пункта {label:?}"))
            .clone();
        let rect = app
            .layout()
            .sidebar_list
            .rect_of(place.id)
            .unwrap_or_else(|| panic!("пункт {label:?} не разложен"));
        app.handle_event(&Event::MouseDown {
            pos: rect.center(),
            button: MouseButton::Left,
        });
        place.path
    }

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pagh-files-nav-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Мёртвых пунктов прежней модели нет: ни `/boot`, ни `/mnt/basic`, а
    /// «Корзина» ведёт в настоящий `Trash/files` (не в `Trash`-каталог) и
    /// «Сеть» — в виртуальную точку, а не в несуществующий каталог VFS.
    #[test]
    fn places_have_no_dead_targets() {
        let app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        let home = app.home();

        let trash = app.places().iter().find(|p| p.label == "Корзина").unwrap();
        assert_eq!(trash.path, format!("{home}/.local/share/Trash/files"));
        assert_eq!(trash.kind, PlaceKind::Trash);

        let network = app.places().iter().find(|p| p.label == "Сеть").unwrap();
        assert_eq!(network.path, "/network");
        assert_eq!(network.kind, PlaceKind::Network);

        for place in app.places() {
            assert_ne!(place.path, "/boot");
            assert_ne!(place.path, "/mnt/basic");
            assert!(!place.path.ends_with("/Trash"), "{}", place.label);
        }

        // Каталоги пользователя и корзина живут под `$HOME`.
        for place in app.places().iter().filter(|p| {
            matches!(
                p.kind,
                PlaceKind::Home | PlaceKind::UserDir | PlaceKind::Trash
            )
        }) {
            assert!(
                place.path == home || place.path.starts_with(&format!("{home}/")),
                "{} вне дома: {}",
                place.label,
                place.path
            );
        }
    }

    /// Клик по каждому пункту «Точек входа» переходит в его каталог, а не
    /// оставляет окно на месте и не показывает ошибку.
    #[test]
    fn sidebar_click_navigates_to_existing_directory() {
        let mut app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        for label in [
            "Домашняя папка",
            "Рабочий стол",
            "Документы",
            "Загрузки",
            "Изображения",
            "Музыка",
            "Видео",
            "Корзина",
        ] {
            let path = click_place(&mut app, label);
            assert_eq!(app.cwd(), path, "{label}");
            assert!(app.error().is_none(), "{label}: {:?}", app.error());
            assert_eq!(app.location_label(), label);
        }
    }

    /// «Сеть» без бэкенда — пустая область с честной строкой, без ошибки
    /// открытия и без выдуманных ресурсов.
    #[test]
    fn network_place_is_empty_and_honest() {
        let mut app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        let path = click_place(&mut app, "Сеть");
        assert_eq!(path, "/network");
        assert_eq!(app.cwd(), "/network");
        assert!(app.error().is_none());
        assert_eq!(app.notice.as_deref(), Some("Нет сетевых ресурсов"));
        assert!(app.entries().is_empty());

        let mut canvas = Canvas::filled(app.size().w, app.size().h, Color::hex(0x101010));
        app.draw(&mut canvas);
    }

    /// Уход из «Сети» в обычный каталог снимает пояснение.
    #[test]
    fn leaving_network_clears_notice() {
        let mut app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        click_place(&mut app, "Сеть");
        assert!(app.notice.is_some());
        click_place(&mut app, "Документы");
        assert!(app.notice.is_none());
    }

    /// На реальной ФС отсутствующие XDG-каталоги создаются при старте, поэтому
    /// клик открывает (пустой) существующий каталог; появившийся файл виден
    /// после перечитывания.
    #[test]
    fn missing_user_dirs_are_created_and_listed() {
        let home = temp_home("create");
        let mut app = FilesApp::new(Box::new(HostVfs::with_home(home.to_string_lossy())), SIZE);
        for name in USER_DIRS {
            let path = home.join(name);
            assert!(path.is_dir(), "не создан {path:?}");
        }
        // Корзина — тоже настоящий каталог (`Trash/files`), созданный при
        // старте, поэтому клик по пункту не показывает ошибку.
        assert!(home.join(".local/share/Trash/files").is_dir());

        let path = click_place(&mut app, "Документы");
        assert_eq!(app.cwd(), path);
        assert!(app.error().is_none(), "{:?}", app.error());
        assert!(app.entries().is_empty(), "новый каталог должен быть пуст");

        std::fs::write(home.join("Documents").join("план.txt"), b"ok").unwrap();
        app.reload();
        let names: Vec<&str> = app.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["план.txt"]);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Несуществующий путь — честная ошибка в области содержимого, а не
    /// фальшивый листинг; отрисовка окна при этом не падает.
    #[test]
    fn missing_directory_reports_error_instead_of_fake_listing() {
        let mut app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        app.navigate_to("/home/user/Нет-такой-папки");
        assert!(app.entries().is_empty());
        assert_eq!(app.error(), Some("объект не найден"));

        let mut canvas = Canvas::filled(app.size().w, app.size().h, Color::hex(0x101010));
        app.draw(&mut canvas);
    }

    /// Двойной клик по каталогу в сетке открывает его: `cwd` меняется, наружу
    /// команда не уходит — навигация внутренняя. Синтез `Event::DoubleClick`
    /// из двух press'ов живёт в `pagh-ui::client` (F3.1).
    #[test]
    fn double_click_directory_navigates_into_it() {
        let mut app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        let index = app
            .entries()
            .iter()
            .position(|entry| entry.name == "Documents")
            .expect("Documents в домашнем каталоге");
        let rect = app.item_rect(index).expect("ячейка сетки");
        let commands = app.handle_event(&Event::DoubleClick { pos: rect.center() });
        assert!(
            commands.is_empty(),
            "навигация не выходит наружу: {commands:?}"
        );
        assert_eq!(app.cwd(), "/home/user/Documents");
        assert!(app.error().is_none(), "{:?}", app.error());
        assert!(app.can_go_back(), "переход попал в историю");
        assert_eq!(app.location_label(), "Документы");
    }

    /// То же на реальной ФС: двойной клик по каталогу входит в него и
    /// показывает его содержимое (FakeVfs-случай не единственный путь).
    #[test]
    fn double_click_real_directory_lists_its_contents() {
        let home = temp_home("dblclick");
        let mut app = FilesApp::new(Box::new(HostVfs::with_home(home.to_string_lossy())), SIZE);
        // Канонические XDG-каталоги создаёт сам Files при старте — файл
        // кладём после этого, чтобы он появился в листинге без ошибок.
        std::fs::write(home.join("Documents").join("план.txt"), b"ok").unwrap();
        let index = app
            .entries()
            .iter()
            .position(|entry| entry.name == "Documents")
            .expect("Documents на реальной ФС");
        let rect = app.item_rect(index).expect("ячейка сетки");
        let commands = app.handle_event(&Event::DoubleClick { pos: rect.center() });
        assert!(
            commands.is_empty(),
            "навигация не выходит наружу: {commands:?}"
        );
        assert_eq!(
            app.cwd(),
            format!("{}/Documents", home.to_string_lossy()),
            "двойной клик открыл каталог"
        );
        let names: Vec<&str> = app.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["план.txt"]);
        assert!(app.error().is_none(), "{:?}", app.error());
        let _ = std::fs::remove_dir_all(home);
    }

    /// «Устройства» — реальные точки монтирования: без `/mnt` пункт не
    /// показывается, корень остаётся.
    #[test]
    fn unmounted_device_is_not_shown() {
        let mut vfs = FakeVfs::new();
        vfs.unlink("/mnt/basic").unwrap();
        vfs.unlink("/mnt").unwrap();
        let app = FilesApp::new(Box::new(vfs), SIZE);
        assert!(app.places().iter().any(|place| place.path == "/"));
        assert!(!app.places().iter().any(|place| place.path == "/mnt"));
        assert!(!app
            .places()
            .iter()
            .any(|place| place.kind == PlaceKind::Device && place.path == "/mnt"));
    }

    /// Тип записи → готовый глиф Fluent: каталог, документ, медиа, устройство.
    /// Это не только выбор картинки: тест ловит регрессию, когда запись снова
    /// начинает рисоваться векторным примитивом.
    #[test]
    fn entry_icons_follow_entry_kind_and_extension() {
        let cases = [
            ("Папка", EntryKind::Directory, FluentIcon::Folder),
            ("notes.md", EntryKind::File, FluentIcon::Document),
            ("photo.png", EntryKind::File, FluentIcon::Image),
            ("PHOTO.JPG", EntryKind::File, FluentIcon::Image),
            ("track01.ogg", EntryKind::File, FluentIcon::MusicNote),
            ("demo.mp4", EntryKind::File, FluentIcon::Video),
            ("pagh.elf", EntryKind::Device, FluentIcon::Drive),
            ("ссылка", EntryKind::Symlink, FluentIcon::Document),
            ("без-расширения", EntryKind::Other, FluentIcon::Document),
        ];
        for (name, kind, expected) in cases {
            let entry = DirEntry::new(name, kind, 0);
            assert_eq!(entry_icon(&entry), expected, "{name}");
            let raster = icons::raster(expected, 32).expect("растр иконки");
            assert!(raster.lit_pixels() > 8, "{name}: пустой растр");
        }
    }

    /// Отрисованное окно действительно содержит готовые растры: в области
    /// иконки папки есть акцентные пиксели, в области файла — приглушённые.
    #[test]
    fn rendered_entries_use_ready_made_rasters() {
        let app = FilesApp::new(Box::new(FakeVfs::concept()), SIZE);
        let mut canvas = Canvas::filled(app.size().w, app.size().h, Color::hex(0x101010));
        app.draw(&mut canvas);
        let palette = app.theme.palette;

        let count = |name: &str, color: Color| {
            let index = app
                .entries()
                .iter()
                .position(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("нет записи {name}"));
            let cell = app.item_rect(index).expect("ячейка");
            let icon = Rect::new(
                cell.x + (cell.w - GRID_ICON_WIDTH) / 2,
                cell.y + GRID_ICON_TOP,
                GRID_ICON_WIDTH,
                GRID_ICON_HEIGHT,
            );
            let mut hits = 0;
            for y in icon.y..icon.bottom() {
                for x in icon.x..icon.right() {
                    if canvas.pixel(Point::new(x, y)) == color {
                        hits += 1;
                    }
                }
            }
            hits
        };

        assert!(
            count("Documents", palette.accent) > 50,
            "иконка каталога не акцентная"
        );
        assert!(
            count("notes.md", palette.text_muted) > 20,
            "иконка документа не приглушённая"
        );
        // Изображение tinted так же, как документ, но маска другая: если бы
        // код рисовал документ для всех файлов, число пикселей совпало бы.
        assert_ne!(
            count("photo.png", palette.text_muted),
            count("notes.md", palette.text_muted)
        );
    }

    /// Смонтированный `/mnt` показывается и открывается.
    #[test]
    fn mounted_device_opens() {
        let mut app = FilesApp::new(Box::new(FakeVfs::new()), SIZE);
        assert!(app.places().iter().any(|place| place.path == "/mnt"));
        let path = click_place(&mut app, "Basic data partition");
        assert_eq!(path, "/mnt");
        assert_eq!(app.cwd(), "/mnt");
        assert!(app.error().is_none(), "{:?}", app.error());
    }
}
