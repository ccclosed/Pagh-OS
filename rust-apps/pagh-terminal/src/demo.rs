//! Built-in demo shell and deterministic screenshot generator.
//!
//! `pagh-terminal` can run as a protocol client with a small interactive demo
//! shell (until a userspace PTY exists), and the same content is rendered
//! headlessly into the committed `screenshots/*.png` fixtures.

use std::io;
use std::path::{Path, PathBuf};

use crate::cell::Rgb;
use crate::client::DEFAULT_SOCKET;
use crate::keys::KeyModes;
use crate::parser::Terminal;
use crate::protocol::mods;
use crate::render::{render_screen, render_window, Canvas, RenderOptions, Theme};

pub const SCREENSHOTS: &[&str] = &["01-welcome", "02-colors", "03-scrollback", "04-unicode"];

/// A tiny line-oriented shell used by the live client demo.  It is not a
/// general shell; it exercises input encoding, parsing and repainting until the
/// kernel shared-console bridge (or a real PTY) is available.
pub struct DemoShell {
    pub terminal: Terminal,
    pub line: String,
    history: Vec<String>,
    history_pos: usize,
    pub quit: bool,
}

impl DemoShell {
    pub fn new(cols: usize, rows: usize) -> DemoShell {
        let mut shell = DemoShell {
            terminal: Terminal::with_scrollback(cols, rows, 2000),
            line: String::new(),
            history: Vec::new(),
            history_pos: 0,
            quit: false,
        };
        shell.write_str(welcome_banner());
        shell.prompt();
        shell
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    pub fn write_str(&mut self, text: &str) {
        self.terminal.feed_str(text);
    }

    pub fn prompt(&mut self) {
        self.write_str("\x1b[1;32mpagh@pagh\x1b[0m:\x1b[1;34m~\x1b[0m$ ");
    }

    /// Echo a command, execute it and print a new prompt.
    pub fn run_line(&mut self, command: &str) {
        self.write_str(command);
        self.write_str("\r\n");
        self.execute(command);
        self.prompt();
    }

    /// Type characters without executing them (used by screenshot fixtures).
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.line.push(ch);
            let mut buf = [0u8; 4];
            self.write(ch.encode_utf8(&mut buf).as_bytes());
        }
    }

    pub fn handle_key(&mut self, code: u32, pressed: bool, modifiers: u32) -> bool {
        if !pressed {
            return false;
        }
        let ctrl = modifiers & mods::CTRL != 0;
        let alt = modifiers & mods::ALT != 0;

        match code {
            crate::keys::evdev::KEY_ENTER => {
                self.write_str("\r\n");
                let line = std::mem::take(&mut self.line);
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                    self.history_pos = self.history.len();
                }
                self.execute(&line);
                if !self.quit {
                    self.prompt();
                }
                true
            }
            crate::keys::evdev::KEY_BACKSPACE => {
                if !ctrl {
                    if self.line.pop().is_some() {
                        self.write_str("\x08 \x08");
                        return true;
                    }
                }
                false
            }
            crate::keys::evdev::KEY_TAB => {
                if !ctrl {
                    self.line.push_str("    ");
                    self.write_str("    ");
                    return true;
                }
                false
            }
            crate::keys::evdev::KEY_UP => {
                if !self.history.is_empty() {
                    if self.history_pos == self.history.len() {
                        self.history_pos = self.history.len() - 1;
                    } else {
                        self.history_pos = self.history_pos.saturating_sub(1);
                    }
                    self.replace_line(self.history[self.history_pos].clone());
                }
                true
            }
            crate::keys::evdev::KEY_DOWN => {
                if !self.history.is_empty() && self.history_pos + 1 < self.history.len() {
                    self.history_pos += 1;
                    self.replace_line(self.history[self.history_pos].clone());
                } else {
                    self.history_pos = self.history.len();
                    self.replace_line(String::new());
                }
                true
            }
            crate::keys::evdev::KEY_LEFT
            | crate::keys::evdev::KEY_RIGHT
            | crate::keys::evdev::KEY_HOME
            | crate::keys::evdev::KEY_END => false,
            _ if ctrl => {
                let bytes = {
                    let modes = KeyModes::from_screen(&self.terminal.screen);
                    crate::keys::encode_key(code, pressed, modifiers, &modes)
                };
                if bytes == [0x0c] {
                    self.write_str("\x1b[2J\x1b[H");
                    self.prompt();
                    let line = self.line.clone();
                    self.write_str(&line);
                    return true;
                }
                if bytes == [0x03] {
                    self.write_str("^C\r\n");
                    self.line.clear();
                    self.prompt();
                    return true;
                }
                if bytes == [0x04] && self.line.is_empty() {
                    self.write_str("logout\r\n");
                    self.quit = true;
                    return true;
                }
                false
            }
            _ => {
                let Some(ch) = crate::keys::text_for_key(code, modifiers) else {
                    return false;
                };
                if ch.is_control() {
                    return false;
                }
                if alt {
                    self.write_str("\x1b");
                }
                self.line.push(ch);
                let mut buf = [0u8; 4];
                self.write(ch.encode_utf8(&mut buf).as_bytes());
                true
            }
        }
    }

    fn replace_line(&mut self, line: String) {
        self.write_str("\r\x1b[K");
        self.prompt();
        self.write_str(&line);
        self.line = line;
    }

    fn execute(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            return;
        };
        match name {
            "help" => self.write_str(
                "\x1b[1;36mcommands\x1b[0m  help, neofetch, ls, colors, utf8, box, echo, clear, scroll, history, quit\r\n",
            ),
            "neofetch" | "about" => self.neofetch(),
            "ls" => self.write_str(
                "\x1b[1;34mDocuments\x1b[0m  \x1b[1;34mDownloads\x1b[0m  \x1b[1;34mPictures\x1b[0m  \x1b[1;34mProjects\x1b[0m  README.md  \x1b[1;32mbuild.sh\x1b[0m\r\n",
            ),
            "colors" => {
                for i in 0..16 {
                    self.write_str(&format!("\x1b[48;5;{i}m  \x1b[0m"));
                }
                self.write_str("\r\n");
                for i in 16..232 {
                    self.write_str(&format!("\x1b[48;5;{i}m \x1b[0m"));
                }
                self.write_str("\r\n");
            }
            "utf8" => self.write_str(
                "UTF-8: русский, Ελληνικά, 你好，世界！, こんにちは, カタカナ, ✓ → █▓▒░\r\n",
            ),
            "box" => self.write_str(
                "┌────────────┬────────────┐\r\n│ terminal   │ VT100/ANSI │\r\n├────────────┼────────────┤\r\n│ colors     │ 16/256/RGB │\r\n└────────────┴────────────┘\r\n",
            ),
            "echo" => {
                let rest = command.trim_start_matches("echo").trim_start();
                self.write_str(rest);
                self.write_str("\r\n");
            }
            "clear" => self.write_str("\x1b[2J\x1b[H"),
            "scroll" => {
                let count = parts
                    .next()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(40)
                    .min(500);
                for i in 0..count {
                    self.write_str(&format!(
                        "\x1b[90m[{i:04}]\x1b[0m generated line {} — Привет, мир!\r\n",
                        i + 1
                    ));
                }
            }
            "history" => {
                let lines: Vec<String> = self
                    .history
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{i:>3}  {line}\r\n"))
                    .collect();
                for line in lines {
                    self.write_str(&line);
                }
            }
            "quit" | "exit" => {
                self.write_str("bye\r\n");
                self.quit = true;
            }
            _ => {
                self.write_str("\x1b[1;31mpagh: command not found:\x1b[0m ");
                self.write_str(name);
                self.write_str("\r\n");
            }
        }
    }

    fn neofetch(&mut self) {
        let art = [
            "\x1b[1;37m   ██████╗  █████╗  ██████╗ ██╗  ██╗\x1b[0m",
            "\x1b[1;37m   ██╔══██╗██╔══██╗██╔════╝ ██║  ██║\x1b[0m",
            "\x1b[1;37m   ██████╔╝███████║██║  ███╗███████║\x1b[0m",
            "\x1b[1;37m   ██╔═══╝ ██╔══██║██║   ██║██╔══██║\x1b[0m",
            "\x1b[1;37m   ██║     ██║  ██║╚██████╔╝██║  ██║\x1b[0m",
            "\x1b[1;37m   ╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝\x1b[0m",
        ];
        let info = [
            "",
            "  \x1b[1;37mOS\x1b[0m        Pagh OS 2.24 (dev)",
            "  \x1b[1;37mKernel\x1b[0m    pagh x86_64",
            "  \x1b[1;37mShell\x1b[0m     pagh-sh",
            "  \x1b[1;37mTerminal\x1b[0m  PaghTerm 0.1 · VT100/ANSI",
            "  \x1b[1;37mFont\x1b[0m      Fira Code 8x18 + Noto CJK",
            "  \x1b[1;37mColors\x1b[0m    16 / 256 / truecolor",
            "  \x1b[1;37mTheme\x1b[0m     breeze-dark",
        ];
        for i in 0..art.len().max(info.len()) {
            if let Some(a) = art.get(i) {
                self.write_str(a);
            } else {
                self.write_str("                                              ");
            }
            if let Some(line) = info.get(i) {
                self.write_str(line);
            }
            self.write_str("\r\n");
        }
    }
}

fn welcome_banner() -> &'static str {
    "\x1b]0;vt0 — Pagh Terminal\x07\
      Pagh OS 2.24  \x1b[90mdesktop terminal — variant B\x1b[0m\r\n\
      \x1b[90mVT100/ANSI · UTF-8 · 16/256/truecolor · scrollback · selection\x1b[0m\r\n\
      \r\n\
      \x1b[90mType \x1b[0mhelp\x1b[90m for the demo commands.\x1b[0m\r\n\
      \r\n"
}

/// Build one of the committed screenshot demos.
pub fn build_demo(name: &str) -> Option<Terminal> {
    match name {
        "01-welcome" => {
            let mut shell = DemoShell::new(90, 28);
            shell.run_line("neofetch");
            shell.run_line("ls");
            shell.run_line("echo \"Привет, мир! 你好，世界！\"");
            shell.type_text("uname -a");
            Some(shell.terminal)
        }
        "02-colors" => Some(colors_demo()),
        "03-scrollback" => Some(scrollback_demo()),
        "04-unicode" => Some(unicode_demo()),
        _ => None,
    }
}

/// Render a named demo with the default theme and window chrome.
pub fn render_demo(name: &str, theme: &Theme) -> Option<Canvas> {
    let terminal = build_demo(name)?;
    let title = if terminal.screen.title.is_empty() {
        "vt0 — Pagh Terminal"
    } else {
        &terminal.screen.title
    };
    let mut opts = RenderOptions::default();
    opts.focused = true;
    opts.blink_on = true;
    Some(render_window(&terminal.screen, theme, title, &opts))
}

/// Render only the terminal viewport (no chrome), for client-surface tests.
pub fn render_demo_viewport(name: &str, theme: &Theme) -> Option<Canvas> {
    let terminal = build_demo(name)?;
    Some(render_screen(
        &terminal.screen,
        theme,
        &RenderOptions::default(),
    ))
}

/// Encode one demo as PNG bytes.
pub fn demo_png(name: &str, theme: &Theme) -> Option<Vec<u8>> {
    Some(crate::png::encode(&render_demo(name, theme)?))
}

/// Generate all committed screenshots into `dir`.
pub fn generate_screenshots(dir: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir)?;
    let theme = Theme::default();
    let mut out = Vec::new();
    for name in SCREENSHOTS {
        let canvas = render_demo(name, &theme).ok_or_else(|| io::Error::other("unknown demo"))?;
        let path = dir.join(format!("{name}.png"));
        canvas.save_png(&path)?;
        out.push(path);
    }
    Ok(out)
}

fn colors_demo() -> Terminal {
    let mut t = Terminal::new(80, 20);
    t.feed_str(
        "\x1b]0;PaghTerm — 256 colors\x07\x1b[?25l\
         \x1b[0m  PaghTerm color coverage  \x1b[90m16 ANSI · 256 xterm · 24-bit RGB\x1b[0m\r\n\r\n",
    );
    // 16 ANSI colors with names.
    for i in 0..16 {
        t.feed(format!("\x1b[48;5;{i}m   \x1b[0m").as_bytes());
    }
    t.feed_str("   \x1b[90m0-15 ANSI\x1b[0m\r\n\r\n");

    // 6x6x6 cube: 36 cells per row.
    for row in 0..6 {
        t.feed_str("  ");
        for col in 0..36 {
            let index = 16 + row * 36 + col;
            t.feed(format!("\x1b[48;5;{index}m \x1b[0m").as_bytes());
        }
        t.feed(
            format!(
                "  \x1b[90m{}-{}\x1b[0m\r\n",
                16 + row * 36,
                16 + row * 36 + 35
            )
            .as_bytes(),
        );
    }
    t.feed_str("\r\n  ");
    for i in 0..24 {
        let index = 232 + i;
        t.feed(format!("\x1b[48;5;{index}m \x1b[0m").as_bytes());
    }
    t.feed_str("  \x1b[90m232-255 gray\x1b[0m\r\n\r\n");

    // 24-bit gradients.
    for row in 0..3 {
        t.feed_str("  ");
        for col in 0..60 {
            let (r, g, b) = match row {
                0 => ((col * 255 / 59) as u8, 40, 255 - (col * 255 / 59) as u8),
                1 => (47, (col * 255 / 59) as u8, 176),
                _ => (255 - (col * 255 / 59) as u8, 212, 176),
            };
            t.feed(format!("\x1b[48;2;{r};{g};{b}m \x1b[0m").as_bytes());
        }
        t.feed_str("\r\n");
    }
    t.feed_str(
        "\r\n  \x1b[1mBold\x1b[0m  \x1b[2mDim\x1b[0m  \x1b[3mItalic\x1b[0m  \x1b[4mUnderline\x1b[0m  \
         \x1b[7mReverse\x1b[0m  \x1b[9mStrike\x1b[0m\r\n",
    );
    t
}

fn scrollback_demo() -> Terminal {
    let mut t = Terminal::with_scrollback(90, 26, 2000);
    t.feed_str(
        "\x1b]0;pagh@pagh: ~/Projects — build log\x07\x1b[?25l\
         \x1b[1;36m  pagh-terminal build — host + static musl\x1b[0m\r\n\
         \x1b[90m  cargo test (host) · cargo build --target x86_64-unknown-linux-musl\x1b[0m\r\n\r\n",
    );
    for i in 0..90 {
        let (tag, color, text) = match i % 9 {
            0 => (
                "TEST",
                "32",
                format!("parser::tests::golden_{:02} ... ok", i),
            ),
            1 => ("TEST", "32", format!("render::tests::cell_{:02} ... ok", i)),
            2 => (
                "WARN",
                "33",
                "unused mut in a test fixture (allowed)".to_string(),
            ),
            3 => ("INFO", "36", format!("screenshot fixture {} rendered", i)),
            4 => (
                "TEST",
                "32",
                format!("protocol::tests::frame_{:02} ... ok", i),
            ),
            _ => (
                "INFO",
                "90",
                format!("compiled pagh_terminal v0.1.0 [{}]", i),
            ),
        };
        t.feed(
            format!("\x1b[90m[{i:03}]\x1b[0m \x1b[{color}m{tag:>4}\x1b[0m {text}\r\n").as_bytes(),
        );
    }
    t.feed_str("\x1b[1;32m  test result: ok. 55 passed; 0 failed\x1b[0m\r\n");
    t.screen.set_view_offset(18);
    // Select a few lines in the middle of the visible region.
    t.screen
        .begin_selection_view(7, 2, crate::screen::SelectionMode::Char);
    t.screen.extend_selection_view(11, 49);
    t
}

fn unicode_demo() -> Terminal {
    let mut t = Terminal::new(80, 22);
    t.feed_str(
        "\x1b]0;PaghTerm — UTF-8 & box drawing\x07\x1b[?25l\
         \x1b[1;36m  UTF-8 + wide characters + box drawing\x1b[0m  \x1b[90mFira Code 8x18 / Noto CJK 16x18\x1b[0m\r\n\r\n",
    );
    let table = [
        (
            "latin / ASCII",
            "The quick brown fox jumps over 13 lazy dogs.",
        ),
        ("кириллица", "Съешь же ещё этих мягких французских булок."),
        ("ελληνικά", "Ξεσκεπάζω την ψυχοφθόρα βδελυγμία."),
        ("中文", "你好，世界！ こんにちは"),
        ("日本語", "こんにちは、世界！ カタカナ"),
        ("symbols", "✓ → ≤ ≥ • ◆ ▒ ▓ █ ╱ ╲ ╳"),
        ("combining", "é (e + U+0301, mark not composed)"),
    ];
    t.feed_str("  ┌");
    t.feed_str(&"─".repeat(20));
    t.feed_str("┬");
    t.feed_str(&"─".repeat(50));
    t.feed_str("┐\r\n");
    for (label, value) in table {
        let label_cells = text_cells(label).min(20);
        let value_cells = text_cells(value).min(49);
        t.feed_str("  │");
        t.feed_str(label);
        t.feed_str(&" ".repeat(20 - label_cells));
        t.feed_str("│ ");
        t.feed_str(value);
        t.feed_str(&" ".repeat(49 - value_cells));
        t.feed_str("│\r\n");
    }
    t.feed_str("  └");
    t.feed_str(&"─".repeat(20));
    t.feed_str("┴");
    t.feed_str(&"─".repeat(50));
    t.feed_str("┘\r\n\r\n");
    t.feed_str(
        "  \x1b[1;36mBlock elements\x1b[0m  ░░▒▒▓▓██  ▏▎▍▌▋▊▉█  ▁▂▃▄▅▆▇█\r\n\
         \x1b[1;36m  ANSI styled\x1b[0m    \x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[33myellow\x1b[0m \x1b[34mblue\x1b[0m \x1b[35mmagenta\x1b[0m \x1b[36mcyan\x1b[0m \x1b[1;97mbold white\x1b[0m\r\n\
         \x1b[1;36m  Wide text\x1b[0m     界 語 かな カナ ＡＢＣ１２３ — два столбца на символ.\r\n\
         \x1b[1;36m  Box + wide\x1b[0m   ┌──┬──┐  │界│語│  └──┴──┘\r\n\r\n\
         \x1b[90m  Parser tests cover scroll regions, alt screen, OSC titles and DEC graphics.\x1b[0m\r\n",
    );
    t
}

fn text_cells(text: &str) -> usize {
    text.chars()
        .map(|c| {
            if crate::font::char_width(c) == 2 {
                2
            } else {
                1
            }
        })
        .sum()
}

/// Convenience for the live client: socket path from the environment or the
/// protocol default.
pub fn socket_path() -> String {
    std::env::var("PAGH_DESKTOP_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

/// The theme's background, exposed for the binary's clear color.
pub fn theme_background(theme: &Theme) -> Rgb {
    theme.bg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demos_build_and_differ() {
        let mut previous: Option<Vec<u8>> = None;
        for name in SCREENSHOTS {
            let png = demo_png(name, &Theme::default()).unwrap();
            assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
            assert!(png.len() > 1000);
            if let Some(prev) = &previous {
                assert_ne!(prev, &png, "screenshots must differ");
            }
            previous = Some(png);
        }
    }

    #[test]
    fn demo_shell_typing_and_enter() {
        let mut shell = DemoShell::new(40, 10);
        assert!(shell.handle_key(crate::keys::evdev::KEY_A, true, 0));
        assert_eq!(shell.line, "a");
        assert!(shell.handle_key(crate::keys::evdev::KEY_ENTER, true, 0));
        assert_eq!(shell.line, "");
        assert!(shell.terminal.text().contains("pagh: command not found"));
    }

    #[test]
    fn demo_shell_handles_input_event_keys() {
        let mut shell = DemoShell::new(40, 10);
        let event = crate::protocol::InputEvent::Key {
            code: crate::keys::evdev::KEY_L,
            pressed: true,
            modifiers: mods::CTRL,
        };
        if let crate::protocol::InputEvent::Key {
            code,
            pressed,
            modifiers,
        } = event
        {
            assert!(shell.handle_key(code, pressed, modifiers));
        }
    }
}
