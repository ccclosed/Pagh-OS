// shell/texttools.rs — чистое потоковое ядро `head`, `tail`, `wc`, `grep`
// 64-битное ядро ОС x86_64 на Rust (#![no_std])
//
// `commands::cmd_cat` вываливает файл целиком; эти четыре команды отвечают на
// вопросы, которые у читателя действительно есть о файле, не влезающем на экран:
// что в начале, что в конце, насколько он велик и какие строки содержат этот
// текст. Все четыре — один и тот же обход байтов файла, поэтому обход живёт
// здесь один раз, а обработчики ниже — тонкий клей VFS + консоли, совсем как
// `cat` (`shell/cat.rs`).
//
// Обход намеренно потоковый: файл читается чанками по 256 байт, так что
// `head -n 1` на логе в 3 GiB стоит одного чанка, а не 3 GiB кучи ядра. `tail`
// держит только последние N строк в ring, `wc` — счётчики, `grep` — ничего.
// Каждый путь ограничен [`MAX_BYTES`], чтобы синтетическое бесконечное устройство
// (`/dev/serial`) не могло крутить команду shell вечно.
//
// Модель строк (та же, что предполагают POSIX text tools):
//   * строка кончается на `\n`;
//   * финальный хвост байтов без newline — последняя строка;
//   * файл, кончающийся `\n`, не имеет лишней пустой строки после;
//   * пустой файл не имеет строк вообще.
// `wc -l` считает символы newline (как Linux), поэтому сходится с этой моделью
// для newline-терминированных файлов и сообщает на один меньше для финальной
// неполной строки — ровно то, что `wc` понимает под «строками».
//
// `head`/`tail` завершают каждую напечатанную строку `\n`, включая финальную
// строку, оставленную файлом без терминатора. Это осознанное отличие от GNU
// coreutils (который копирует последнюю строку сырой): shell печатает следующий
// промпт сразу после команды, так что незавершённая последняя строка приклеила
// бы `pagh:/>` к тексту файла и испортила бы транскрипт, который читает
// E2E-скрипт или человек — та же логика, что у терминатора вывода `cat`.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::commands::{resolve_arg, shell_print_bytes, shell_println};
use super::registry::ShellCtx;

/// Кап байт на одну команду: достаточно для любого реального текстового файла
/// в этой системе и мало настолько, чтобы бесконечное устройство не заклинило shell.
pub(crate) const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Размер запроса на чтение. Мал намеренно: потоковым инструментам не нужен
/// 4-KiB стек-буфер для того, чему `head -n 3` обычно требует 20 байт.
const CHUNK: usize = 256;

/// Что сделал один обход. `bytes` — сколько файла потреблено (весь файл, если
/// вызывающий не остановил раньше или не упёрся в [`MAX_BYTES`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Walked {
    pub bytes: u64,
    /// Виденные строки, где `newline_terminated` считает только строки,
    /// окончившиеся `\n` (смысл `wc -l`).
    pub lines: u64,
    pub newline_terminated: u64,
    pub read_failed: bool,
    /// Обход остановился, потому что `on_line` попросил, а не на EOF/капе.
    pub stopped_early: bool,
}

/// Обойти файл построчно.
///
/// `read(offset, buf)` кладёт не более `buf.len()` байт в `buf` и возвращает
/// количество (`Ok(0)` — EOF) или `Err(())`. `on_line` получает номер строки с 1,
/// была ли она newline-терминирована, и байты строки (терминатор не входит в
/// срез); возврат `false` останавливает обход.
pub(crate) fn walk<R, F>(mut read: R, mut on_line: F) -> Walked
where
    R: FnMut(u64, &mut [u8]) -> Result<usize, ()>,
    F: FnMut(u64, bool, &[u8]) -> bool,
{
    let mut out = Walked::default();
    let mut buf = [0u8; CHUNK];
    let mut line: Vec<u8> = Vec::new();
    while out.bytes < MAX_BYTES {
        let want = ((MAX_BYTES - out.bytes).min(CHUNK as u64)) as usize;
        match read(out.bytes, &mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                let n = n.min(want);
                if n == 0 {
                    break;
                }
                out.bytes += n as u64;
                for &b in &buf[..n] {
                    if b == b'\n' {
                        out.lines += 1;
                        out.newline_terminated += 1;
                        if !on_line(out.lines, true, &line) {
                            out.stopped_early = true;
                            return out;
                        }
                        line.clear();
                    } else {
                        line.push(b);
                    }
                }
            }
            Err(()) => {
                out.read_failed = true;
                break;
            }
        }
    }
    // Не–терминированный хвост всё равно строка (если он не пустой).
    if !line.is_empty() {
        out.lines += 1;
        let _ = on_line(out.lines, false, &line);
    }
    out
}

/// Написать одну строку, всегда с завершающим newline, чтобы файл, чья последняя
/// строка без терминатора, всё равно оставил следующий промпт на своей строке.
fn emit<W: FnMut(&[u8])>(write: &mut W, line: &[u8]) {
    write(line);
    write(b"\n");
}

/// Первые `limit` строк файла.
pub(crate) fn head<R, W>(limit: usize, read: R, mut write: W) -> Walked
where
    R: FnMut(u64, &mut [u8]) -> Result<usize, ()>,
    W: FnMut(&[u8]),
{
    walk(read, |n, _, line| {
        if n as usize > limit {
            return false;
        }
        emit(&mut write, line);
        (n as usize) < limit
    })
}

/// Последние `limit` строк файла.
///
/// Ring держит `limit` строк; строка длиннее всего файла всё равно копируется
/// один раз, так что память ограничена размером файла и числом строк, а не
/// длиной файла в строках.
pub(crate) fn tail<R, W>(limit: usize, read: R, mut write: W) -> Walked
where
    R: FnMut(u64, &mut [u8]) -> Result<usize, ()>,
    W: FnMut(&[u8]),
{
    let mut ring: VecDeque<Vec<u8>> = VecDeque::new();
    let walked = walk(read, |_, _, line| {
        if limit > 0 {
            ring.push_back(line.to_vec());
            if ring.len() > limit {
                ring.pop_front();
            }
        }
        true
    });
    for line in ring.iter() {
        emit(&mut write, line);
    }
    walked
}

/// Счётчики `wc` в порядке, в котором их печатает Linux.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub lines: u64,
    pub words: u64,
    pub bytes: u64,
    pub read_failed: bool,
}

/// Считать строки, слова через пробелы и байты.
///
/// Слова — те же пробеги, что считает Linux `wc`: максимальные пробеги
/// непробельных символов, где пробел — набор C `isspace` (`space, \t, \n, \v, \f, \r`).
pub(crate) fn word_count<R>(read: R) -> Counts
where
    R: FnMut(u64, &mut [u8]) -> Result<usize, ()>,
{
    let mut counts = Counts::default();
    let walked = walk(read, |_, _, line| {
        let mut in_word = false;
        for &b in line {
            let space = matches!(b, b' ' | b'\t' | 0x0b | 0x0c | b'\r');
            if space {
                in_word = false;
            } else if !in_word {
                in_word = true;
                counts.words += 1;
            }
        }
        true
    });
    counts.lines = walked.newline_terminated;
    counts.bytes = walked.bytes;
    counts.read_failed = walked.read_failed;
    counts
}

/// Переключатели `grep`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GrepOpts {
    /// `-i`: ASCII case-insensitive сопоставление.
    pub ignore_case: bool,
    /// `-n`: префикс каждой совпавшей строки её номером с 1.
    pub line_numbers: bool,
    /// `-c`: печатать только число совпавших строк.
    pub count_only: bool,
    /// `-v`: печатать строки, которые НЕ совпадают.
    pub invert: bool,
}

/// Что нашёл один вызов `grep`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Grepped {
    pub matched: u64,
    pub walked: Walked,
}

/// Свернуть регистр одного ASCII-байта (`-i` только ASCII, как `grep` без локали).
fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// Содержит ли `haystack` подстроку `needle`?
fn contains(haystack: &[u8], needle: &[u8], ignore_case: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let eq = |a: &[u8], b: &[u8]| {
        if ignore_case {
            a.iter().zip(b.iter()).all(|(x, y)| fold(*x) == fold(*y))
        } else {
            a == b
        }
    };
    haystack.windows(needle.len()).any(|w| eq(w, needle))
}

/// Напечатать каждую строку, совпадающую с `pattern` (или, с `invert`, не совпадающую).
pub(crate) fn grep<R, W>(pattern: &[u8], opts: GrepOpts, read: R, mut write: W) -> Grepped
where
    R: FnMut(u64, &mut [u8]) -> Result<usize, ()>,
    W: FnMut(&[u8]),
{
    let mut out = Grepped::default();
    let walked = walk(read, |n, _, line| {
        let hit = contains(line, pattern, opts.ignore_case) != opts.invert;
        if hit {
            out.matched += 1;
            if !opts.count_only {
                if opts.line_numbers {
                    let prefix = format!("{}:", n);
                    write(prefix.as_bytes());
                }
                emit(&mut write, line);
            }
        }
        true
    });
    out.walked = walked;
    out
}

// ─── тонкий клей VFS + консоли ────────────────────────────────────────────────

/// Прочитать `path` через `body`, переводя «not found»/«is a directory»/ошибки
/// чтения в стиль однострочной диагностики shell.
fn with_file<F>(cmd: &str, path_arg: &str, mut body: F)
where
    F: FnMut(&dyn crate::vfs::VfsNode),
{
    let path = resolve_arg(path_arg);
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            if node.is_directory() {
                shell_println(&format!("{}: {}: is a directory", cmd, path));
            } else {
                body(&*node);
            }
        }
        Err(_) => shell_println(&format!("{}: {}: not found", cmd, path)),
    }
}

/// Разобрать аргументы `head`/`tail`: опциональный `-n N` / `-nN`, затем ровно
/// один путь. Возвращает `(лимит строк, путь)`.
fn parse_line_args(args: &[&str], default: usize) -> Option<(usize, String)> {
    let mut limit = default;
    let mut path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if let Some(rest) = a.strip_prefix("-n") {
            let digits = if rest.is_empty() {
                i += 1;
                if i >= args.len() {
                    return None;
                }
                args[i]
            } else {
                rest
            };
            limit = digits.parse::<usize>().ok()?;
        } else if a.starts_with('-') && a.len() > 1 {
            return None;
        } else if path.is_none() {
            path = Some((*a).into());
        } else {
            return None;
        }
        i += 1;
    }
    path.map(|p| (limit, p))
}

/// `head [-n LINES] FILE`: напечатать первые LINES строк (по умолчанию 10).
pub(super) fn cmd_head(_ctx: &mut ShellCtx, args: &[&str]) {
    let Some((limit, path)) = parse_line_args(args, 10) else {
        shell_println("head: usage: head [-n LINES] FILE");
        return;
    };
    with_file("head", &path, |node| {
        let out = head(
            limit,
            |off, buf| node.read(off, buf).map_err(|_| ()),
            shell_print_bytes,
        );
        if out.read_failed {
            shell_println(&format!("head: {}: read error", path));
        }
    });
}

/// `tail [-n LINES] FILE`: напечатать последние LINES строк (по умолчанию 10).
pub(super) fn cmd_tail(_ctx: &mut ShellCtx, args: &[&str]) {
    let Some((limit, path)) = parse_line_args(args, 10) else {
        shell_println("tail: usage: tail [-n LINES] FILE");
        return;
    };
    with_file("tail", &path, |node| {
        let out = tail(
            limit,
            |off, buf| node.read(off, buf).map_err(|_| ()),
            shell_print_bytes,
        );
        if out.read_failed {
            shell_println(&format!("tail: {}: read error", path));
        }
    });
}

/// `wc [-l] [-w] [-c] FILE`: считать строки, слова, байты (все три по умолчанию).
pub(super) fn cmd_wc(_ctx: &mut ShellCtx, args: &[&str]) {
    let (mut want_lines, mut want_words, mut want_bytes) = (false, false, false);
    let mut path: Option<String> = None;
    for a in args {
        match *a {
            "-l" => want_lines = true,
            "-w" => want_words = true,
            "-c" => want_bytes = true,
            other if other.starts_with('-') && other.len() > 1 => {
                shell_println(&format!("wc: unknown option: {}", other));
                return;
            }
            other => {
                if path.is_some() {
                    shell_println("wc: usage: wc [-l| -w| -c] FILE");
                    return;
                }
                path = Some(other.into());
            }
        }
    }
    let Some(path) = path else {
        shell_println("wc: usage: wc [-l| -w| -c] FILE");
        return;
    };
    if !want_lines && !want_words && !want_bytes {
        want_lines = true;
        want_words = true;
        want_bytes = true;
    }
    with_file("wc", &path, |node| {
        let counts = word_count(|off, buf| node.read(off, buf).map_err(|_| ()));
        let mut cols: Vec<String> = Vec::new();
        if want_lines {
            cols.push(format!("{}", counts.lines));
        }
        if want_words {
            cols.push(format!("{}", counts.words));
        }
        if want_bytes {
            cols.push(format!("{}", counts.bytes));
        }
        shell_println(&format!("{} {}", cols.join(" "), path));
        if counts.read_failed {
            shell_println(&format!("wc: {}: read error", path));
        }
    });
}

/// `grep [-i] [-n] [-c] [-v] PATTERN FILE`: напечатать строки, содержащие PATTERN.
pub(super) fn cmd_grep(_ctx: &mut ShellCtx, args: &[&str]) {
    let mut opts = GrepOpts::default();
    let mut pattern: Option<String> = None;
    let mut path: Option<String> = None;
    for a in args {
        match *a {
            "-i" => opts.ignore_case = true,
            "-n" => opts.line_numbers = true,
            "-c" => opts.count_only = true,
            "-v" => opts.invert = true,
            "-in" | "-ni" => {
                opts.ignore_case = true;
                opts.line_numbers = true;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                shell_println(&format!("grep: unknown option: {}", other));
                return;
            }
            _ => {
                if pattern.is_none() {
                    pattern = Some((*a).into());
                } else if path.is_none() {
                    path = Some((*a).into());
                } else {
                    shell_println("grep: only one FILE is supported");
                    return;
                }
            }
        }
    }
    let (Some(pattern), Some(path)) = (pattern, path) else {
        shell_println("grep: usage: grep [-i] [-n] [-c] PATTERN FILE");
        return;
    };
    with_file("grep", &path, |node| {
        let out = grep(
            pattern.as_bytes(),
            opts,
            |off, buf| node.read(off, buf).map_err(|_| ()),
            shell_print_bytes,
        );
        if opts.count_only {
            shell_println(&format!("{}", out.matched));
        }
        if out.walked.read_failed {
            shell_println(&format!("grep: {}: read error", path));
        }
    });
}
