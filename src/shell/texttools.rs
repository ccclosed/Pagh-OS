// shell/texttools.rs — the pure streaming core of `head`, `tail`, `wc`, `grep`
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// `commands::cmd_cat` dumps a whole file; these four commands answer the
// questions a reader actually has about a file that does not fit on the screen:
// what is at the top, what is at the bottom, how big is it, and which lines
// contain this text. All four are the same walk over the file's bytes, so the
// walk lives here once and the handlers below are thin VFS + console glue,
// exactly like `cat` (`shell/cat.rs`).
//
// The walk is streaming on purpose: a file is read in 256-byte chunks, so
// `head -n 1` on a 3 GiB log costs one chunk, not 3 GiB of kernel heap. `tail`
// keeps only the last N lines in a ring, `wc` keeps counters, and `grep` keeps
// nothing. Every path is capped at [`MAX_BYTES`] so a synthetic endless device
// (`/dev/serial`) cannot spin a shell command forever.
//
// Line model (the same one POSIX text tools assume):
//   * a line ends at `\n`;
//   * a final run of bytes that is not newline-terminated is the last line;
//   * a file that ends with `\n` has no extra empty line after it;
//   * an empty file has no lines at all.
// `wc -l` counts newline characters (like Linux), so it agrees with this model
// for newline-terminated files and reports one less for a final partial line —
// which is exactly what `wc` means by "lines".
//
// `head`/`tail` terminate every printed line with `\n`, including a final line
// the file left unterminated. That is one deliberate difference from GNU
// coreutils (which copies the last line raw): the shell prints the next prompt
// immediately after the command, so an unterminated last line would glue
// `pagh:/>` onto the file's text and corrupt any transcript an E2E script or a
// human reads — the same reasoning as `cat`'s transcript terminator.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::commands::{resolve_arg, shell_print_bytes, shell_println};
use super::registry::ShellCtx;

/// Byte cap for one command: enough for any real text file on this system,
/// small enough that an endless device cannot wedge the shell.
pub(crate) const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Read request size. Small on purpose: the streaming tools should not need a
/// 4 KiB stack buffer for what `head -n 3` usually needs 20 bytes of.
const CHUNK: usize = 256;

/// What one walk did. `bytes` is how much of the file was consumed (the whole
/// file unless a caller stopped early or hit [`MAX_BYTES`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Walked {
    pub bytes: u64,
    /// Lines seen, where `newline_terminated` counts only lines that ended in
    /// `\n` (the `wc -l` meaning).
    pub lines: u64,
    pub newline_terminated: u64,
    pub read_failed: bool,
    /// The walk stopped because `on_line` asked it to, not at EOF/cap.
    pub stopped_early: bool,
}

/// Walk a file line by line.
///
/// `read(offset, buf)` places at most `buf.len()` bytes at `buf` and returns
/// the count (`Ok(0)` is EOF) or `Err(())`. `on_line` receives the 1-based line
/// number, whether the line was newline-terminated, and the line's bytes (the
/// terminator is not part of the slice); it returns `false` to stop the walk.
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
    // The unterminated tail is still a line (unless it is empty).
    if !line.is_empty() {
        out.lines += 1;
        let _ = on_line(out.lines, false, &line);
    }
    out
}

/// Write one line, always newline-terminated, so a file whose last line has no
/// terminator still leaves the next prompt on its own row.
fn emit<W: FnMut(&[u8])>(write: &mut W, line: &[u8]) {
    write(line);
    write(b"\n");
}

/// First `limit` lines of a file.
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

/// Last `limit` lines of a file.
///
/// The ring holds `limit` lines; a line longer than the whole file is still
/// copied once, so memory is bounded by the file's size and the line count, not
/// by the file's length in lines.
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

/// `wc` counters, in the order Linux prints them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub lines: u64,
    pub words: u64,
    pub bytes: u64,
    pub read_failed: bool,
}

/// Count lines, whitespace-separated words and bytes.
///
/// Words are the same runs Linux `wc` counts: maximal runs of non-whitespace,
/// where whitespace is the C `isspace` set (`space, \t, \n, \v, \f, \r`).
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

/// `grep` switches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GrepOpts {
    /// `-i`: ASCII case-insensitive match.
    pub ignore_case: bool,
    /// `-n`: prefix each match with its 1-based line number.
    pub line_numbers: bool,
    /// `-c`: print only the count of matching lines.
    pub count_only: bool,
    /// `-v`: print lines that do NOT match.
    pub invert: bool,
}

/// What one `grep` call found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Grepped {
    pub matched: u64,
    pub walked: Walked,
}

/// Case-fold one ASCII byte (`-i` is ASCII-only, like `grep` without a locale).
fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// Does `haystack` contain `needle`?
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

/// Print every line that matches (or, with `invert`, does not match) `pattern`.
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

// ─── thin VFS + console glue ────────────────────────────────────────────────

/// Read `path` through `body`, translating "not found"/"is a directory"/read
/// errors into the shell's one-line-diagnostic style.
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

/// Parse `head`/`tail` arguments: optional `-n N` / `-nN`, then exactly one
/// path. Returns `(line limit, path)`.
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

/// `head [-n LINES] FILE`: print the first LINES lines (default 10).
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

/// `tail [-n LINES] FILE`: print the last LINES lines (default 10).
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

/// `wc [-l] [-w] [-c] FILE`: count lines, words, bytes (all three by default).
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

/// `grep [-i] [-n] [-c] [-v] PATTERN FILE`: print lines containing PATTERN.
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
