//! `pagh-terminal --shell-child`: отдельный userspace shell за pipe'ами.
//!
//! Это **не** kernel-shell: команды ядра (`df`, `ps`, `pkg`, `lxrun`, …) живут в
//! консольной задаче ядра и доступны только на boot-VT. Здесь — маленький
//! интерпретатор строк для терминального окна: он читает команды из stdin,
//! печатает результат в stdout и держит собственный cwd, поэтому годится для
//! сессии «окно ↔ процесс» без управляющего терминала (PTY/termios — следующий
//! этап). Именно поэтому в `unknown command` честно сказано, что команда не
//! встроена, а не подразумевается, что её нет в системе.
//!
//! Ввод редактируется здесь же: pipe не эхо-терминал, поэтому shell обязан сам
//! печатать набранное, обрабатывать Backspace/Ctrl+U/Ctrl+C и не засорять
//! вывод escape-последовательностями клавиш со стрелками (они отбрасываются).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Ширина «экрана» для колонок `ls`: у pipe нет размера окна, а строка шире 80
/// колонок в терминальном окне всё равно переносится.
const TERM_WIDTH: usize = 80;

/// Минимальный shell одной терминальной сессии.
pub struct Shell {
    cwd: PathBuf,
}

impl Shell {
    /// Shell, унаследовавший cwd процесса терминала.
    pub fn new() -> Shell {
        Shell {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        }
    }

    /// Shell с явным cwd — так тесты не трогают cwd всего процесса.
    pub fn with_cwd(cwd: impl Into<PathBuf>) -> Shell {
        Shell { cwd: cwd.into() }
    }

    /// Главный цикл: промпт, строка, команда, снова промпт.
    pub fn run(&mut self) -> i32 {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut input = stdin.lock();
        let mut raw_out = stdout.lock();
        let mut out = CrlfWriter::new(&mut raw_out);
        let mut line = Line::new();
        let mut escape = Escape::None;
        let mut pending_cr = false;

        if self.write_prompt(&mut out).is_err() {
            return 0;
        }
        let mut buf = [0u8; 256];
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            for &byte in &buf[..n] {
                // CRLF из внешнего источника — один Enter, а не два.
                if pending_cr && byte == b'\n' {
                    pending_cr = false;
                    continue;
                }
                pending_cr = byte == b'\r';

                // Хвост escape-последовательности (стрелки, Home/End) не ввод:
                // у сессии нет истории/курсора, а ESC-байты в строке сломали бы
                // эхо и последующую команду.
                if escape != Escape::None {
                    escape = escape.step(byte);
                    continue;
                }
                if byte == 0x1b {
                    escape = Escape::Esc;
                    continue;
                }

                match byte {
                    b'\r' | b'\n' => {
                        let _ = out.write_all(b"\r\n");
                        let text = String::from_utf8_lossy(line.bytes()).into_owned();
                        line.clear();
                        if let Some(code) = self.execute(&text, &mut out) {
                            return code;
                        }
                        if self.write_prompt(&mut out).is_err() {
                            return 0;
                        }
                    }
                    0x7f | 0x08 => {
                        if line.backspace() {
                            let _ = out.write_all(b"\x08 \x08");
                        }
                    }
                    0x15 => {
                        // Ctrl+U — стереть строку целиком.
                        let width = line.width();
                        for _ in 0..width {
                            let _ = out.write_all(b"\x08 \x08");
                        }
                        line.clear();
                    }
                    0x03 => {
                        // Ctrl+C: строку в мусор, но сессию не закрываем.
                        let _ = out.write_all(b"^C\r\n");
                        line.clear();
                        if self.write_prompt(&mut out).is_err() {
                            return 0;
                        }
                    }
                    0x04 => {
                        // Ctrl+D на пустой строке — выход, как в настоящем shell.
                        if line.is_empty() {
                            let _ = out.write_all(b"\r\n");
                            return 0;
                        }
                    }
                    0x00..=0x1f => {}
                    _ => {
                        line.push(byte);
                        let _ = out.write_all(&[byte]);
                    }
                }
                let _ = out.flush();
            }
            let _ = out.flush();
        }
        0
    }

    fn write_prompt(&self, out: &mut impl Write) -> io::Result<()> {
        write!(out, "pagh:{}> ", self.display_cwd())?;
        out.flush()
    }

    /// Короткая форма cwd для промпта: `/` остаётся `/`, остальное — как есть.
    fn display_cwd(&self) -> String {
        let text = self.cwd.display().to_string();
        if text.is_empty() {
            "/".to_string()
        } else {
            text
        }
    }

    /// Выполнить одну строку. `Some(code)` — команда `exit`.
    ///
    /// Отдельный `out` вместо прямого stdout — чтобы тесты читали ровно тот
    /// байтовый поток, который ушёл бы в pipe окна.
    pub fn execute(&mut self, line: &str, out: &mut dyn Write) -> Option<i32> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }
        let result = match parts[0] {
            "help" => self.help(out),
            "ls" => self.ls(parts.get(1).copied(), out),
            "cd" => self.cd(parts.get(1).copied(), out),
            "pwd" => self.pwd(out),
            "echo" => {
                let text = parts[1..].join(" ");
                writeln!(out, "{text}")
            }
            "cat" => self.cat(&parts[1..], out),
            "uname" => writeln!(out, "Pagh-OS x86_64 Linux-compat (terminal session shell)"),
            "clear" => out.write_all(b"\x1b[2J\x1b[H"),
            "exit" => {
                let code = parts
                    .get(1)
                    .and_then(|value| value.parse::<i32>().ok())
                    .unwrap_or(0);
                return Some(code);
            }
            other => writeln!(
                out,
                "{other}: command not found in this terminal session (try 'help'; \
                 kernel-shell commands such as df/ps/lxrun are not wired to this window yet)"
            ),
        };
        match result {
            Ok(()) => None,
            Err(error) => {
                let _ = writeln!(out, "pagh-shell: {error}");
                None
            }
        }
    }

    fn help(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(
            out,
            "pagh terminal session shell — built-in commands:\n\
             \x20 help           this list\n\
             \x20 ls [DIR]       list a directory (default: cwd)\n\
             \x20 cd [DIR]       change directory (default: /)\n\
             \x20 pwd            print the working directory\n\
             \x20 echo ARGS...   print the arguments\n\
             \x20 cat FILE...    print file contents\n\
             \x20 uname [-a]     OS name\n\
             \x20 clear          clear the screen\n\
             \x20 exit [CODE]    end the shell session\n\
             Kernel-shell commands (df, ps, pkg, lxrun, …) are not available here yet: \
             this is a separate userspace process behind pipes, not the console shell."
        )
    }

    fn pwd(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{}", self.display_cwd())
    }

    fn cd(&mut self, arg: Option<&str>, out: &mut dyn Write) -> io::Result<()> {
        let target = self.resolve(arg.unwrap_or("/"));
        match std::fs::metadata(&target) {
            Ok(meta) if meta.is_dir() => {
                // Храним абсолютный путь, чтобы `cd ..` и `pwd` не зависели от
                // cwd самого процесса терминала.
                self.cwd = std::fs::canonicalize(&target).unwrap_or(target);
                Ok(())
            }
            Ok(_) => writeln!(out, "cd: {}: not a directory", target.display()),
            Err(error) => writeln!(out, "cd: {}: {error}", target.display()),
        }
    }

    fn ls(&self, arg: Option<&str>, out: &mut dyn Write) -> io::Result<()> {
        let target = self.resolve(arg.unwrap_or("."));
        let read = match std::fs::read_dir(&target) {
            Ok(read) => read,
            Err(error) => return writeln!(out, "ls: {}: {error}", target.display()),
        };
        let mut names: Vec<String> = Vec::new();
        for entry in read.flatten() {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            names.push(name);
        }
        names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)));
        for line in columns(&names, TERM_WIDTH) {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }

    fn cat(&self, args: &[&str], out: &mut dyn Write) -> io::Result<()> {
        if args.is_empty() {
            return writeln!(out, "cat: missing file operand");
        }
        for arg in args {
            let target = self.resolve(arg);
            match std::fs::read(&target) {
                Ok(bytes) => {
                    out.write_all(&bytes)?;
                    if !bytes.ends_with(b"\n") {
                        out.write_all(b"\n")?;
                    }
                }
                Err(error) => writeln!(out, "cat: {}: {error}", target.display())?,
            }
        }
        Ok(())
    }

    /// Абсолютный путь для аргумента команды (cwd сессии, не процесса).
    fn resolve(&self, arg: &str) -> PathBuf {
        let path = Path::new(arg);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

/// Состояние разбора ESC-последовательности: всё, что начинается с ESC, для
/// этой сессии не ввод (стрелки/функциональные клавиши приходят как `ESC [ …`).
#[derive(Clone, Copy, PartialEq)]
enum Escape {
    None,
    Esc,
    Csi,
    CsiParam,
}

impl Escape {
    fn step(self, byte: u8) -> Escape {
        match self {
            Escape::Esc => {
                if byte == b'[' {
                    Escape::Csi
                } else {
                    Escape::None
                }
            }
            // Первый байт после `ESC [`: параметр или сразу финальный символ.
            Escape::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    Escape::None
                } else {
                    Escape::CsiParam
                }
            }
            // …дальше параметры до финального байта `@`..`~`.
            Escape::CsiParam => {
                if (0x40..=0x7e).contains(&byte) {
                    Escape::None
                } else {
                    Escape::CsiParam
                }
            }
            Escape::None => Escape::None,
        }
    }
}

/// Редактируемая строка ввода. Хранит байты, а не `String`: pipe может отдать
/// UTF-8 по частям, и решение о валидности принимает команда, а не echo.
struct Line {
    bytes: Vec<u8>,
}

impl Line {
    fn new() -> Line {
        Line { bytes: Vec::new() }
    }

    fn clear(&mut self) {
        self.bytes.clear();
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    /// Удалить последний символ: для многобайтного UTF-8 — всю последовательность,
    /// иначе Backspace оставил бы битые байты.
    fn backspace(&mut self) -> bool {
        // Continuation byte means the popped codepoint was multi-byte: strip the
        // rest of it (continuations, then the lead byte). An ASCII byte is a
        // whole character on its own and must not pull the previous one with it.
        let Some(popped) = self.bytes.pop() else {
            return false;
        };
        if (0x80..=0xbf).contains(&popped) {
            while self
                .bytes
                .last()
                .is_some_and(|byte| (0x80..=0xbf).contains(byte))
            {
                self.bytes.pop();
            }
            if self.bytes.last().is_some_and(|byte| *byte >= 0xc0) {
                self.bytes.pop();
            }
        }
        true
    }

    /// Сколько колонок занимает набранное (для Ctrl+U): считаем символы, а не
    /// байты; ширина восточно-азиатских символов здесь не моделируется.
    fn width(&self) -> usize {
        let mut width = 0;
        let mut index = 0;
        while index < self.bytes.len() {
            let byte = self.bytes[index];
            let len = if byte < 0x80 {
                1
            } else if byte >> 5 == 0b110 {
                2
            } else if byte >> 4 == 0b1110 {
                3
            } else if byte >> 3 == 0b11110 {
                4
            } else {
                1
            };
            index += len;
            width += 1;
        }
        width
    }
}

/// Разложить имена в колонки не шире `width` (пробелы между колонками — 2).
fn columns(names: &[String], width: usize) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    let cell = names.iter().map(String::len).max().unwrap_or(0) + 2;
    let per_row = (width / cell.max(1)).max(1);
    let rows = names.len().div_ceil(per_row);
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..per_row {
            let Some(name) = names.get(row + col * rows) else {
                break;
            };
            if !line.is_empty() {
                let pad = cell.saturating_sub(line.len() % cell);
                line.push_str(&" ".repeat(pad));
            }
            line.push_str(name);
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Перевод `\n` в `\r\n` на выходе shell-потомка.
///
/// Pipe — не терминал: дисциплины строк (`OPOST`/`ONLCR`) здесь нет, поэтому
/// одиночный `\n` опускает курсор вниз, оставляя колонку, и каждая следующая
/// строка уезжает вправо. Настоящий tty пишет `\r\n` сам — здесь это делает
/// эта обёртка, чтобы вывод ложился в сетку окна ровно.
struct CrlfWriter<'a, W: Write> {
    inner: &'a mut W,
    last_cr: bool,
}

impl<'a, W: Write> CrlfWriter<'a, W> {
    fn new(inner: &'a mut W) -> CrlfWriter<'a, W> {
        CrlfWriter {
            inner,
            last_cr: false,
        }
    }
}

impl<W: Write> Write for CrlfWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = Vec::with_capacity(buf.len() + 8);
        for &byte in buf {
            if byte == b'\n' && !self.last_cr {
                out.push(b'\r');
            }
            out.push(byte);
            self.last_cr = byte == b'\r';
        }
        self.inner.write_all(&out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Точка входа режима `--shell-child`.
pub fn run() -> i32 {
    Shell::new().run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_line(shell: &mut Shell, line: &str) -> String {
        let mut out = Vec::new();
        assert!(shell.execute(line, &mut out).is_none());
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn help_lists_the_builtins() {
        let mut shell = Shell::with_cwd("/");
        let text = run_line(&mut shell, "help");
        for command in [
            "help", "ls", "cd", "pwd", "echo", "cat", "uname", "clear", "exit",
        ] {
            assert!(text.contains(command), "help misses {command}: {text}");
        }
        assert!(
            text.contains("not available here"),
            "help must be honest about kernel commands"
        );
    }

    #[test]
    fn echo_and_pwd_use_the_session_state() {
        let mut shell = Shell::with_cwd("/mnt");
        assert_eq!(
            run_line(&mut shell, "echo hello world").trim_end(),
            "hello world"
        );
        assert_eq!(run_line(&mut shell, "pwd").trim_end(), "/mnt");
    }

    #[test]
    fn ls_lists_real_entries_and_appends_slashes() {
        let dir = std::env::temp_dir().join(format!("pagh-sh-ls-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        std::fs::write(dir.join("file.txt"), b"x").expect("write file");
        let mut shell = Shell::with_cwd(&dir);
        let text = run_line(&mut shell, "ls");
        assert!(
            text.contains("sub/"),
            "directories need a trailing slash: {text}"
        );
        assert!(text.contains("file.txt"), "files missing: {text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cd_changes_only_the_session_directory() {
        let dir = std::env::temp_dir().join(format!("pagh-sh-cd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).expect("mkdir");
        let mut shell = Shell::with_cwd(&dir);
        assert_eq!(run_line(&mut shell, "cd inner"), "");
        assert_eq!(
            run_line(&mut shell, "pwd").trim_end(),
            dir.join("inner").display().to_string()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_command_names_itself_and_the_kernel_gap() {
        let mut shell = Shell::with_cwd("/");
        let text = run_line(&mut shell, "df");
        assert!(text.contains("df: command not found"), "{text}");
        assert!(text.contains("kernel-shell commands"), "{text}");
    }

    #[test]
    fn exit_returns_its_status() {
        let mut shell = Shell::with_cwd("/");
        let mut out = Vec::new();
        assert_eq!(shell.execute("exit 7", &mut out), Some(7));
        assert_eq!(shell.execute("exit", &mut out), Some(0));
    }

    #[test]
    fn backspace_drops_a_whole_utf8_sequence() {
        let mut line = Line::new();
        for byte in "привет".bytes() {
            line.push(byte);
        }
        assert!(line.backspace());
        assert_eq!(String::from_utf8_lossy(line.bytes()), "приве");
        assert_eq!(line.width(), 5);
    }

    #[test]
    fn arrow_keys_are_swallowed_by_the_escape_parser() {
        let mut escape = Escape::None;
        for byte in b"\x1b[A" {
            escape = if *byte == 0x1b {
                Escape::Esc
            } else {
                escape.step(*byte)
            };
        }
        assert!(escape == Escape::None);
    }

    #[test]
    fn columns_wrap_at_the_requested_width() {
        let names: Vec<String> = (0..10).map(|index| format!("file{index}")).collect();
        let lines = columns(&names, 20);
        assert!(lines.len() > 1, "expected multiple rows: {lines:?}");
        assert!(lines.iter().all(|line| line.len() <= 20), "{lines:?}");
    }
}
