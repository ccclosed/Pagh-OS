//! Рендеринг консоли: палитра цветов и раскрашенный вывод строк/промпта.
//!
//! У framebuffer один глобальный foreground-цвет (ставится через
//! [`crate::drivers::framebuffer::set_fg_color`]); у serial понятия цвета нет.
//! Поэтому каждый хелпер здесь раскрашивает только framebuffer, а serial-поток
//! оставляет простым — никогда не выводя цвет или escape-байты в serial (R8.5).
//! Каждая раскрашенная печать во framebuffer обёрнута так, что foreground-цвет
//! всегда сбрасывается в `Default` после, защищая от застревания консоли в
//! цвете (R8.2/8.3/8.4).
//!
//! Поскольку ядро собирается с `panic = "abort"`, поймать панику и выполнить
//! reset на unwind нельзя; вместо этого контракт — строгая последовательность
//! set -> вызов -> вернуть цвет, самый надёжный из доступных guard'ов.

#![allow(dead_code)]

use crate::drivers::framebuffer;

/// Стиль вывода консоли. Каждый отображается в один foreground-цвет framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Обычный текст и пользовательский ввод.
    Default,
    /// Интерактивный промпт (R8.3).
    Prompt,
    /// Строки ошибок (R8.2).
    Error,
    /// Строки успеха/подтверждения (R8.4).
    Success,
}

/// Активные цвета, по одному на [`Style`]. Засеяны историческими значениями,
/// чтобы нетронутая загрузка выглядела ровно как до появления тем.
static PALETTE: crate::sync::spinlock::Spinlock<[u32; 4]> =
    crate::sync::spinlock::Spinlock::new([0xFFFFFF, 0x55FF55, 0xFF5555, 0x55FFFF]);

/// Foreground-цвет (0xRRGGBB) обычного текста и пользовательского ввода (R8.6).
pub fn color_default() -> u32 {
    PALETTE.lock()[0]
}
/// Foreground-цвет промпта (R8.3).
pub fn color_prompt() -> u32 {
    PALETTE.lock()[1]
}
/// Foreground-цвет строк ошибок (R8.2).
pub fn color_error() -> u32 {
    PALETTE.lock()[2]
}
/// Foreground-цвет строк успеха/подтверждения (R8.4).
pub fn color_success() -> u32 {
    PALETTE.lock()[3]
}

/// Заменить всю палитру — это вызывает `shell::theme::apply`. Один лок, так что
/// читатель никогда не увидит половину новой темы.
pub fn set_colors(text: u32, prompt: u32, error: u32, success: u32) {
    *PALETTE.lock() = [text, prompt, error, success];
}

impl Style {
    /// Foreground-цвет framebuffer (0xRRGGBB) для этого стиля.
    pub fn color(self) -> u32 {
        match self {
            Style::Default => color_default(),
            Style::Prompt => color_prompt(),
            Style::Error => color_error(),
            Style::Success => color_success(),
        }
    }
}

/// Выполнить `f` с foreground-цветом framebuffer, равным цвету `style`, затем
/// всегда вернуть цвет в `Default`.
///
/// Последовательность set -> вызов -> reset выполняется безусловно. Ядро
/// использует `panic = "abort"`, поэтому unwind ловить нечего: паника в `f` в
/// любом случае оборвёт ядро; на всех непаникующих путях цвет гарантированно
/// восстанавливается, так что консоль не застревает в цвете (R8.2/8.3/8.4).
/// Serial не трогается — цвет только у framebuffer (R8.5).
pub fn with_style(style: Style, f: impl FnOnce()) {
    framebuffer::set_fg_color(style.color());
    f();
    framebuffer::set_fg_color(color_default());
}

/// Напечатать `text` полной строкой цветом `style` на framebuffer (с
/// последующим сбросом в `Default`) и просто на serial. Serial выводится первым,
/// чтобы serial-поток оставался чистым и никогда не видел байтов цвета (R8.5).
fn styled_line(style: Style, text: &str) {
    crate::kprintln!("{}", text);
    with_style(style, || {
        crate::fb_println!("{}", text);
    });
}

/// Напечатать `text` строкой цветом [`Style::Error`] на framebuffer (с
/// последующим сбросом) и просто на serial (R8.2, R8.5).
pub fn error_line(text: &str) {
    styled_line(Style::Error, text);
}

/// Напечатать `text` строкой цветом [`Style::Success`] на framebuffer (с
/// последующим сбросом) и просто на serial (R8.4, R8.5).
pub fn success_line(text: &str) {
    styled_line(Style::Success, text);
}

/// Напечатать промпт shell `pagh:{cwd}> ` (без завершающего newline) цветом
/// [`Style::Prompt`] на framebuffer (с последующим сбросом) и просто на serial
/// (R8.3, R8.5).
pub fn prompt(cwd: &str) {
    crate::kprint!("pagh:{}> ", cwd);
    with_style(Style::Prompt, || {
        crate::fb_print!("pagh:{}> ", cwd);
    });
}
