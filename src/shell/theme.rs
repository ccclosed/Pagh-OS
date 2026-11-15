//! Настройка терминала: цветовые темы, обои и живая панель `settings`.
//!
//! Всё здесь косметическое и обратимое. У консоли одна ручка, которая реально
//! важна, — foreground-цвет (`framebuffer::set_fg_color`), — плюс ручка фона и
//! слой обоев; модуль владеет выбором поверх них. Три правила не дают ему стать
//! источником «аппаратных» багов:
//!
//! * **Состояние — одна маленькая структура под одним локом.** [`settings()`]
//!   выдаёт guard; всё, что нужно читать несколько полей, читает их вместе, так
//!   что полуприменённую тему наблюдать нельзя.
//! * **Применение явное.** Мутация структуры не трогает экран; [`apply`]
//!   проталкивает палитру в драйверы, а вызывающие перерисовывают. Панель,
//!   сменившая пять цветов и затем упавшая, иначе оставила бы консоль полосатой.
//! * **Никаких аллокаций на пути рисования.** Обои считаются из пиксельных
//!   координат, а не из сохранённого изображения, так что градиент 1280×800
//!   стоит арифметики, а смена `settings` не стоит ничего.
//!
//! Панель ([`run_panel`]) — модальный экран, а не команда shell: она владеет
//! клавиатурой, пока пользователь не выйдет, потому что каждое нажатие там
//! означает «поменяй и покажи», а именно это построчный редактор shell иначе бы
//! проглотил. `paint` придерживается того же подхода.

use crate::shell::keys::{Decoder, KeyEvent};
use crate::sync::spinlock::Spinlock;

/// Встроенные цветовые схемы. Индекс в этой таблице — то, что хранят команда
/// `theme` и панель, так что добавление схемы — одна строка здесь.
pub static THEMES: &[Theme] = &[
    Theme {
        name: "classic",
        text: 0xFF_FFFF,
        prompt: 0x55_FF55,
        error: 0xFF_5555,
        success: 0x55_FFFF,
        background: 0x00_0000,
        status_bar: 0x1B_3A5B,
        status_text: 0xE6_F0FF,
    },
    Theme {
        name: "amber",
        text: 0xFF_B000,
        prompt: 0xFF_D75F,
        error: 0xFF_3B30,
        success: 0xFF_E08A,
        background: 0x14_0C00,
        status_bar: 0x3A_2400,
        status_text: 0xFF_E0A0,
    },
    Theme {
        name: "green",
        text: 0x33_FF66,
        prompt: 0x9A_FF33,
        error: 0xFF_4444,
        success: 0x33_FFCC,
        background: 0x00_0A00,
        status_bar: 0x10_3A18,
        status_text: 0xCC_FFCC,
    },
    Theme {
        name: "paper",
        text: 0x1A_1A1A,
        prompt: 0x00_55AA,
        error: 0xC0_2A20,
        success: 0x00_7744,
        background: 0xF2_EFE6,
        status_bar: 0xD8_D2C4,
        status_text: 0x2A_2A2A,
    },
    Theme {
        name: "synth",
        text: 0xF8_F8F2,
        prompt: 0xFF_2A9D,
        error: 0xFF_4D6D,
        success: 0x2A_FFE0,
        background: 0x1A_1030,
        status_bar: 0x3A_1A5A,
        status_text: 0xFF_D0F0,
    },
];

/// Одна цветовая схема. Все значения — `0xRRGGBB`, как у framebuffer-драйвера.
#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub text: u32,
    pub prompt: u32,
    pub error: u32,
    pub success: u32,
    pub background: u32,
    pub status_bar: u32,
    pub status_text: u32,
}

/// Слой обоев, рисуемый за текстом.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wallpaper {
    /// Обычный цвет фона.
    None,
    /// Вертикальный градиент между двумя цветами, производными от темы.
    Gradient,
    /// Горизонтальный градиент.
    GradientX,
    /// Градиент плюс более тёмная полоса каждую третью строку развёртки (в духе CRT).
    Scanlines,
    /// Детерминированное звёздное поле из фиксированного хеша — без состояния RNG.
    Stars,
    /// Более крупная шахматка — удобно видеть, какие именно клетки перерисовываются.
    Grid,
}

impl Wallpaper {
    pub const ALL: [Wallpaper; 6] = [
        Wallpaper::None,
        Wallpaper::Gradient,
        Wallpaper::GradientX,
        Wallpaper::Scanlines,
        Wallpaper::Stars,
        Wallpaper::Grid,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Wallpaper::None => "none",
            Wallpaper::Gradient => "gradient",
            Wallpaper::GradientX => "gradient-x",
            Wallpaper::Scanlines => "scanlines",
            Wallpaper::Stars => "stars",
            Wallpaper::Grid => "grid",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.name() == s)
    }
}

/// Всё состояние кастомизации.
#[derive(Clone, Copy)]
pub struct Settings {
    pub theme: usize,
    pub wallpaper: Wallpaper,
    /// Текст/фон status bar берутся из темы; этот флаг вместо этого скрывает полосу.
    pub status_bar: bool,
    /// Показывать позицию мыши в status bar (`mouse(x,y)`).
    pub status_mouse: bool,
    /// Рисуется ли мигающая каретка в shell.
    pub caret: bool,
}

impl Settings {
    const fn new() -> Self {
        Settings {
            theme: 0,
            wallpaper: Wallpaper::None,
            status_bar: true,
            status_mouse: true,
            caret: true,
        }
    }

    /// Активная тема.
    pub fn theme(&self) -> &'static Theme {
        &THEMES[self.theme.min(THEMES.len() - 1)]
    }

    /// Переключиться на следующую схему и вернуть её имя.
    pub fn next_theme(&mut self) -> &'static str {
        self.theme = (self.theme + 1) % THEMES.len();
        self.theme().name
    }
}

static SETTINGS: Spinlock<Settings> = Spinlock::new(Settings::new());

/// Ставится, когда палитра сменилась под ногами shell.
///
/// Модель консоли помнит цвет каждой клетки, так что смена палитры делает
/// неверными пиксели *за* текстом, а не сам текст: перерисовка пикселей
/// показывает новый фон под старыми цветами. Shell потребляет этот флаг и
/// перепечатывает свою строку — единственное место, которое может заново выдать
/// промпт в новых цветах.
static FORCE_REDRAW: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Попросить shell перепечатать промпт и перечитать status bar на следующем проходе.
pub fn request_redraw() {
    FORCE_REDRAW.store(true, core::sync::atomic::Ordering::SeqCst);
}

/// Забрать ожидающий запрос перерисовки (и очистить его).
pub fn take_force_redraw() -> bool {
    FORCE_REDRAW.swap(false, core::sync::atomic::Ordering::SeqCst)
}

/// Выполнить `f` с настройками. Держите замыкание коротким: лок маскирует прерывания.
pub fn with<R>(f: impl FnOnce(&mut Settings) -> R) -> R {
    let mut guard = SETTINGS.lock();
    f(&mut guard)
}

/// Снимок для вызывающих, которым нужно несколько полей сразу без удержания лока.
pub fn snapshot() -> Settings {
    *SETTINGS.lock()
}

/// Протолкнуть палитру в драйверы. Не перерисовывает: вызывающий решает, когда
/// экран догонит, так что многошаговая смена никогда не показывается наполовину применённой.
pub fn apply() {
    let s = snapshot();
    let theme = s.theme();
    crate::drivers::framebuffer::set_fg_color(theme.text);
    crate::drivers::framebuffer::set_bg_color(theme.background);
    // У полосы в теме свои два цвета; вывод их из цветов текста делал полосу
    // каждой схемы похожей на дефолтную.
    crate::drivers::framebuffer::set_status_colors(theme.status_bar, theme.status_text);
    // Обои видны *сквозь* текст, только когда консоль перестаёт штамповать свой
    // фон за каждым глифом. На обычном фоне этот штамп и держит перепечатанную
    // клетку чистой, так что его надо вернуть.
    crate::drivers::framebuffer::set_opaque_bg(s.wallpaper == Wallpaper::None);
    crate::shell::render::set_colors(theme.text, theme.prompt, theme.error, theme.success);
    // Status strip рисуется своими двумя цветами, и его текст сам по себе не
    // меняется вместе с темой, поэтому его надо перерисовать здесь — shell
    // перерисовывает полосу только когда меняется её *текст*, а это может быть
    // секундами позже.
    let mouse = crate::drivers::ps2_mouse::poll();
    crate::drivers::framebuffer::draw_status_bar(&status_left(), &status_right(&mouse));
    // Всё остальное на экране нарисовано старой палитрой и неверно.
    request_redraw();
}

/// `pagh OS   <cwd>` — левая половина полосы. Публична, потому что её делит
/// собственный путь перерисовки shell (см. `shell::shell_status_text`).
pub fn status_left() -> alloc::string::String {
    alloc::format!("pagh OS   {}", crate::shell::path::cwd())
}

/// `up Ns   mouse(x,y)` — правая половина полосы, с необязательным показанием мыши.
pub fn status_right(mouse: &crate::drivers::ps2_mouse::MouseState) -> alloc::string::String {
    let secs = crate::task::scheduler::ticks() / crate::arch::x86_64::apic::TICK_HZ;
    let s = snapshot();
    // Прокрученная консоль сообщает об этом в полосе: без этого нет видимой
    // разницы между «читаю историю» и «shell перестал печатать».
    let (view, history) = crate::drivers::framebuffer::scroll_state();
    let scroll = if view > 0 {
        alloc::format!("   scroll {}/{}", view, history)
    } else {
        alloc::string::String::new()
    };
    if !s.status_mouse {
        return alloc::format!("up {}s{}", secs, scroll);
    }
    alloc::format!(
        "up {}s   mouse({},{}){}",
        secs,
        mouse.x / crate::drivers::framebuffer::CHAR_WIDTH,
        mouse.y / crate::drivers::framebuffer::CHAR_HEIGHT,
        scroll
    )
}

/// Перерисовать всю поверхность для текущих настроек: обои, затем сетку
/// консоли, которая была на экране. Вызывается после смены темы или обоев.
pub fn repaint() {
    let s = snapshot();
    let theme = s.theme();
    // Обои владеют только текстовой областью; status strip держит свои два
    // цвета, поэтому он перерисовывается явно после замены фона.
    // Скрыть стрелку на всю перерисовку: иначе её закрасят обои/сетка, а её
    // сохранённый фон будет описывать кадр, которого больше нет (см. контракт
    // порядка в drivers::cursor).
    crate::drivers::cursor::hide();
    // Порядок важен. `repaint_grid` заново выдаёт каждую клетку, а клетка несёт
    // фон, которым была нарисована последней (чёрный на свежей консоли), — так
    // что он кладёт старый непрозрачный фон поверх картинки. Обои, нарисованные
    // ПОСЛЕДНИМИ, и делают их видимыми внутри текстовой сетки, а не только на
    // полях, которые сетка не покрывает; глифы, напечатанные отсюда, прозрачны
    // (`set_opaque_bg(false)` в `apply`), так что текст остаётся на картинке.
    crate::drivers::framebuffer::repaint_grid();
    crate::drivers::framebuffer::with(|fb| paint_wallpaper(fb, s.wallpaper, theme));
    crate::drivers::framebuffer::repaint_grid();
    let mouse = crate::drivers::ps2_mouse::poll();
    crate::drivers::framebuffer::draw_status_bar(&status_left(), &status_right(&mouse));
    crate::drivers::cursor::show();
}

/// Заполнить текстовую область (всё выше status bar) обоями.
///
/// Обои считаются на каждый пиксель из `(x, y)` и темы, так что не требуют
/// сохранённого изображения и не могут разойтись с палитрой.
fn paint_wallpaper(
    fb: &mut crate::drivers::framebuffer::FramebufferWriter,
    wallpaper: Wallpaper,
    theme: &Theme,
) {
    let (w, h) = fb.dimensions();
    let bottom = h.saturating_sub(crate::drivers::framebuffer::STATUS_BAR_HEIGHT);
    let bg = theme.background;
    match wallpaper {
        Wallpaper::None => fb.fill_rect(0, 0, w, bottom, bg),
        Wallpaper::Gradient | Wallpaper::Scanlines => {
            // Рампа идёт от тонированного оттенка к самому цвету фона.
            // Масштабирование фона (очевидная реализация) ничего не делает на
            // чёрной теме — 0x000000 на что угодно всё равно чёрный, так что classic
            // не показывала обоев вообще. Смешивание *к* фиксированному оттенку
            // работает на любой теме; на светлой тот же оттенок затемняет — так и
            // должен выглядеть «градиент» там.
            // Светлый конец должен быть явно светлее фона, иначе «градиент» —
            // двухцветная рампа, которую никто не видит: 21% серого в чёрном — это
            // 0x0A0C0F, что читается как чёрный на любом мониторе.
            let tint = if brightness(bg) < 400 {
                0x8A_9CC8
            } else {
                0x14_1820
            };
            let top = mix(bg, tint, 96);
            let floor = if brightness(bg) < 400 {
                mix(bg, tint, 12)
            } else {
                bg
            };
            for y in 0..bottom {
                let t = y as u32 * 256 / bottom.max(1) as u32;
                let mut color = mix(top, floor, t);
                if wallpaper == Wallpaper::Scanlines && y % 3 == 2 {
                    color = shade(color, 0.55);
                }
                fb.fill_rect(0, y, w, 1, color);
            }
        }
        Wallpaper::GradientX => {
            let tint = if brightness(bg) < 400 {
                0x8A_9CC8
            } else {
                0x14_1820
            };
            let left = mix(bg, tint, 96);
            for x in 0..w {
                let t = x as u32 * 256 / w.max(1) as u32;
                fb.fill_rect(x, 0, 1, bottom, mix(left, bg, t));
            }
        }
        Wallpaper::Stars => {
            fb.fill_rect(0, 0, w, bottom, bg);
            let tint = 0xB0_BCD8;
            let bright = mix(bg, tint, 200);
            let dim = mix(bg, tint, 96);
            // Хеш, а не RNG: поле одинаково после каждой перерисовки, так что
            // смена темы не заставляет небо прыгать.
            let mut i = 0u32;
            while i < 900 {
                let hx = hash(i.wrapping_mul(2654435761));
                let hy = hash(hx ^ 0x9E3779B9);
                let x = (hx >> 8) as usize % w.max(1);
                let y = (hy >> 8) as usize % bottom.max(1);
                let c = if hx & 0x40 == 0 { bright } else { dim };
                fb.set_pixel(x, y, c);
                i += 1;
            }
        }
        Wallpaper::Grid => {
            // Сначала слабое поле, затем линии: на чёрной теме линии, нарисованные
            // прямо по фону, — 0x0A0A0C и просто невидимы.
            fb.fill_rect(0, 0, w, bottom, mix(bg, 0x8A_9CC8, 40));
            let line = mix(bg, 0xB0_BCD8, 130);
            let (cw, ch) = (
                crate::drivers::framebuffer::CHAR_WIDTH,
                crate::drivers::framebuffer::CHAR_HEIGHT,
            );
            let mut x = 0;
            while x < w {
                fb.fill_rect(x, 0, 1, bottom, line);
                x += cw;
            }
            let mut y = 0;
            while y < bottom {
                fb.fill_rect(0, y, w, 1, line);
                y += ch;
            }
        }
    }
}

/// Воспринимаемая яркость (0..765), нужна, чтобы решить, осветлять обоям фон
/// темы или затемнять.
fn brightness(color: u32) -> u32 {
    ((color >> 16) & 0xFF) + ((color >> 8) & 0xFF) + (color & 0xFF)
}

/// Умножить каналы цвета с клампом в 0xFF. `f < 1` затемняет, `f > 1` осветляет.
fn shade(color: u32, f: f32) -> u32 {
    let ch = |shift: u32| {
        let v = ((color >> shift) & 0xFF) as f32 * f;
        let v = if v > 255.0 { 255.0 } else { v };
        v as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Линейное смешивание: `t = 0` даёт `a`, `t = 256` даёт `b`.
fn mix(a: u32, b: u32, t: u32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
        (x * (256 - t) + y * t) >> 8
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Дешёвый целочисленный хеш для звёздного поля.
fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    x
}

// ─── панель `settings` ───

/// Одна строка панели: метка, текущее значение и как его менять.
struct Row {
    label: &'static str,
    value: alloc::string::String,
}

/// Запустить интерактивную панель настроек, пока пользователь не выйдет.
///
/// Клавиши: `↑`/`↓` или `k`/`j` — движение, `←`/`→` или `h`/`l` — изменение,
/// `t` — циклическая смена темы, `w` — обоев, `r` — сброс, `q`/`Esc`/`Enter` —
/// выход. Каждое изменение применяется и перерисовывается сразу, так что панель
/// и есть preview.
pub fn run_panel() {
    let mut sel = 0usize;
    let mut decoder = Decoder::new();
    loop {
        // Панель — полноэкранный кадр: скрыть стрелку, пока он рисуется, затем
        // вернуть её поверх готового кадра (тот же контракт, что у `paint`).
        crate::drivers::cursor::hide();
        draw_panel(sel);
        crate::drivers::framebuffer::flush();
        let mouse = crate::drivers::ps2_mouse::poll();
        crate::drivers::cursor::move_to(mouse.x, mouse.y);
        // Спать, пока действительно не придёт сканкод. Голый `halt()`
        // возвращается на каждом тике таймера, а перерисовка полноэкранной панели
        // плюс flush всей поверхности на 1 кГц тратила бы CPU на кадры, которых
        // никто не просил.
        while !crate::drivers::ps2_kbd::has_scancode() {
            crate::arch::cpu::halt();
            // Указатель продолжает работать над панелью во время ожидания клавиши.
            let mouse = crate::drivers::ps2_mouse::poll();
            crate::drivers::cursor::move_to(mouse.x, mouse.y);
        }
        let Some(bytes) = crate::drivers::get_char("keyboard").and_then(|k| k.read_char()) else {
            continue;
        };
        let Some(ev) = decoder.feed(bytes) else {
            continue;
        };
        let mut changed = false;
        match ev {
            KeyEvent::Up | KeyEvent::Char('k') => sel = sel.saturating_sub(1),
            KeyEvent::Down | KeyEvent::Char('j') => sel = (sel + 1).min(ROW_COUNT - 1),
            KeyEvent::Left | KeyEvent::Char('h') => changed = adjust(sel, -1),
            KeyEvent::Right | KeyEvent::Char('l') => changed = adjust(sel, 1),
            KeyEvent::Char('t') => {
                with(|s| s.next_theme());
                changed = true;
            }
            KeyEvent::Char('w') => {
                with(|s| {
                    let i = Wallpaper::ALL
                        .iter()
                        .position(|w| *w == s.wallpaper)
                        .unwrap_or(0);
                    s.wallpaper = Wallpaper::ALL[(i + 1) % Wallpaper::ALL.len()];
                });
                changed = true;
            }
            KeyEvent::Char('r') => {
                with(|s| *s = Settings::new());
                changed = true;
            }
            KeyEvent::Escape | KeyEvent::Char('q') | KeyEvent::Enter => break,
            _ => {}
        }
        if changed {
            apply();
            repaint();
        }
    }
    // Выход из панели: вернуть консоль в вид, который ждёт shell, со свежим
    // промптом на очищенном экране.
    apply();
    crate::drivers::framebuffer::clear_screen();
    crate::shell::render::prompt(&crate::shell::path::cwd());
}

const ROW_COUNT: usize = 5;

fn adjust(row: usize, dir: i32) -> bool {
    with(|s| match row {
        0 => {
            let n = THEMES.len();
            s.theme = if dir > 0 {
                (s.theme + 1) % n
            } else {
                (s.theme + n - 1) % n
            };
            true
        }
        1 => {
            let all = Wallpaper::ALL;
            let i = all.iter().position(|w| *w == s.wallpaper).unwrap_or(0);
            let n = all.len();
            s.wallpaper = all[if dir > 0 {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            }];
            true
        }
        2 => {
            s.status_bar = !s.status_bar;
            true
        }
        3 => {
            s.status_mouse = !s.status_mouse;
            true
        }
        4 => {
            s.caret = !s.caret;
            true
        }
        _ => false,
    })
}

fn rows() -> [Row; ROW_COUNT] {
    let s = snapshot();
    [
        Row {
            label: "Theme",
            value: alloc::string::String::from(s.theme().name),
        },
        Row {
            label: "Wallpaper",
            value: alloc::string::String::from(s.wallpaper.name()),
        },
        Row {
            label: "Status bar",
            value: on_off(s.status_bar),
        },
        Row {
            label: "Status: mouse",
            value: on_off(s.status_mouse),
        },
        Row {
            label: "Caret",
            value: on_off(s.caret),
        },
    ]
}

fn on_off(v: bool) -> alloc::string::String {
    alloc::string::String::from(if v { "on" } else { "off" })
}

/// Нарисовать панель: рамка-список с подсвеченной выбранной строкой.
fn draw_panel(sel: usize) {
    let s = snapshot();
    let theme = s.theme();
    let rows = rows();
    let (w, h) = crate::drivers::framebuffer::dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let cw = crate::drivers::framebuffer::CHAR_WIDTH;
    let chh = crate::drivers::framebuffer::CHAR_HEIGHT;
    let panel_w = (cw * 34).min(w.saturating_sub(8));
    let panel_h = (chh * (ROW_COUNT + 4)).min(h.saturating_sub(8));
    let x = (w - panel_w) / 2;
    let y = (h - panel_h) / 2;

    crate::drivers::framebuffer::with(|fb| {
        fb.fill_rect(x, y, panel_w, panel_h, shade(theme.background, 1.5));
        fb.draw_rect(x, y, panel_w, panel_h, theme.prompt);
        fb.draw_text_px(
            x + cw / 2,
            y + chh / 2,
            "terminal settings",
            theme.prompt,
            shade(theme.background, 1.5),
        );
        for (i, row) in rows.iter().enumerate() {
            let ry = y + chh * (i + 2);
            let bg = if i == sel {
                shade(theme.prompt, 0.25)
            } else {
                shade(theme.background, 1.5)
            };
            fb.fill_rect(x + cw / 2, ry, panel_w - cw, chh, bg);
            fb.draw_text_px(x + cw / 2, ry, row.label, theme.text, bg);
            fb.draw_text_px(
                x + panel_w - cw / 2 - row.value.len() * cw,
                ry,
                &row.value,
                theme.prompt,
                bg,
            );
        }
        let help_y = y + panel_h - chh - chh / 2;
        fb.draw_text_px(
            x + cw / 2,
            help_y,
            "up/down pick - left/right change - t theme - w wallpaper - r reset - q quit",
            theme.text,
            shade(theme.background, 1.5),
        );
    });
}
