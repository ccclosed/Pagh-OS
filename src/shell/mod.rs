// shell/mod.rs — Interactive command-line shell
// 64-bit x86_64 OS kernel in Rust (#![no_std])

// Submodule landing spots for the user-friendly-shell feature. Each is a pure
// or thin-I/O unit; `mod.rs` owns the only place that touches the keyboard,
// framebuffer, serial, and VFS.
mod commands;
// The pure-logic submodules are exposed `pub(crate)` so the in-kernel property
// suite in `src/test.rs` can exercise them directly (design properties P21–P27
// and the registry/render/path unit tests). They remain crate-private — nothing
// leaves the crate. `commands` stays private (it is thin VFS/console I/O).
pub(crate) mod complete;
pub(crate) mod editor;
pub(crate) mod history;
pub(crate) mod keys;
pub(crate) mod path;
pub(crate) mod registry;
pub(crate) mod render;
pub(crate) mod suggest;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Print a string to both consoles (serial + framebuffer) with no newline.
fn print_both(s: &str) {
    crate::kprint!("{}", s);
    crate::fb_print!("{}", s);
}

/// Destructively erase `n` visible characters to the left on both consoles.
///
/// The framebuffer console only supports a destructive backspace (`0x08`,
/// which moves the column back one and blanks that cell); it has no
/// non-destructive cursor positioning. The classic `backspace, space,
/// backspace` triplet erases one rendered glyph and leaves the column one to
/// the left. Serial terminals interpret the same sequence sensibly.
fn erase_visible(n: usize) {
    for _ in 0..n {
        crate::kprint!("\x08 \x08");
        crate::fb_print!("\x08 \x08");
    }
}

/// Number of visible columns a string occupies on the console.
///
/// Keyboard input is ASCII, so one `char` maps to one rendered column. This
/// drives the erase count in [`redraw_line`].
fn visible_len(s: &str) -> usize {
    s.chars().count()
}

/// Redraw the current input region so the console reflects the editor buffer
/// exactly (R1.6).
///
/// `shown` tracks how many visible characters of input are currently rendered
/// after the prompt. We destructively erase that whole region, then reprint the
/// full current buffer and update `shown`.
///
/// LIMITATION (v1): the framebuffer is destructive-only — there is no
/// non-destructive glyph-level cursor positioning and `\r` would jump back over
/// the prompt. So the *visible* caret always rests at end-of-line, while the
/// editor's *logical* cursor (used for insert/delete) may sit mid-line. The
/// reprinted buffer always shows the correct content; only the blinking-caret
/// position is approximate. Mid-line edits therefore appear to apply "at the
/// end" visually but are placed correctly in the buffer.
fn redraw_line(editor: &editor::LineEditor, shown: &mut usize) {
    erase_visible(*shown);
    let buf = editor.buffer();
    print_both(buf);
    *shown = visible_len(buf);
}

/// Drop to a fresh line, re-render the prompt, and reprint the current buffer.
/// Used after listing Tab-completion candidates (R3.4). Resets `shown` to the
/// reprinted buffer width.
fn reprompt_with_buffer(editor: &editor::LineEditor, shown: &mut usize) {
    crate::kprintln!();
    crate::fb_println!();
    render::prompt(&path::cwd());
    let buf = editor.buffer();
    print_both(buf);
    *shown = visible_len(buf);
}

/// List completion candidates on a new line, then re-render the prompt+buffer.
fn list_candidates(editor: &editor::LineEditor, shown: &mut usize, candidates: &[String]) {
    crate::kprintln!();
    crate::fb_println!();
    let joined = candidates.join("  ");
    crate::kprintln!("{}", joined);
    crate::fb_println!("{}", joined);
    reprompt_with_buffer(editor, shown);
}

/// Execute a command line by dispatching through the command registry.
///
/// The line is tokenized with `split_whitespace`: `parts[0]` is the command
/// name and `parts[1..]` are the arguments passed to the handler. An
/// empty/whitespace-only line is a no-op. Unknown commands name the offending
/// token (R7.1) and, when a near match exists, append a "did you mean" hint
/// (R7.2).
fn execute_command(cmd: &str) {
    let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    let name = parts[0];
    let args = &parts[1..];

    match registry::lookup(name) {
        Some(spec) => {
            let mut ctx = registry::ShellCtx::new();
            (spec.handler)(&mut ctx, args);
        }
        None => {
            // Unknown command: name the token (R7.1), then offer the nearest
            // registry command within a small edit-distance threshold (R7.2).
            render::error_line(&format!("Unknown command: '{}'", name));
            let names: Vec<&str> = registry::command_names().collect();
            if let Some(suggestion) = suggest::nearest_command(name, &names, 2) {
                render::error_line(&format!("did you mean '{}'?", suggestion));
            }
        }
    }
}

/// Handle a Tab keypress: complete the token under the cursor (R3.3/3.4/3.5).
///
/// Completion target selection (per task 10.1): if the line has no whitespace
/// (a single/first token) it is completed against command names; otherwise the
/// text after the final whitespace is treated as a path token and completed
/// against the VFS entries of its parent directory. VFS reads happen here and
/// are passed to the pure `complete` functions as data — the completion logic
/// itself performs no I/O.
fn handle_tab(editor: &mut editor::LineEditor, shown: &mut usize) {
    let line = String::from(editor.buffer());

    match line.rfind(|c: char| c == ' ' || c == '\t') {
        // No whitespace -> first/only token: complete a command name.
        None => {
            apply_completion(editor, shown, "", "", complete::complete_command(&line));
        }
        // Whitespace present -> complete the trailing token as a path.
        Some(idx) => {
            let before = &line[..=idx]; // command + args + separating space
            let token = &line[idx + 1..]; // the path partial being completed

            // Split the token into its directory part (up to and including the
            // last '/') and the segment being completed.
            let dir_part = match token.rfind('/') {
                Some(i) => &token[..=i],
                None => "",
            };

            // Resolve the parent directory against the CWD and read its
            // entries from the VFS (as data for the pure completer).
            let cwd = path::cwd();
            let parent = if dir_part.is_empty() {
                cwd.clone()
            } else {
                path::resolve(&cwd, dir_part)
            };

            let entries_owned: Vec<String> = match crate::vfs::lookup_path(&parent) {
                Ok(node) => match node.readdir() {
                    Ok(children) => children.iter().map(|c| String::from(c.name())).collect(),
                    Err(_) => Vec::new(),
                },
                Err(_) => Vec::new(),
            };
            let entry_refs: Vec<&str> = entries_owned.iter().map(|s| s.as_str()).collect();

            let comp = complete::complete_path(&cwd, token, &entry_refs);
            apply_completion(editor, shown, before, dir_part, comp);
        }
    }
}

/// Apply a [`complete::Completion`] to the editor.
///
/// `prefix` is the text preceding the token being completed (empty for command
/// completion); `dir_part` is the leading directory portion of a path token
/// that must be preserved in front of the completed segment (empty for command
/// completion, where the completion already carries the full token).
fn apply_completion(
    editor: &mut editor::LineEditor,
    shown: &mut usize,
    prefix: &str,
    dir_part: &str,
    comp: complete::Completion,
) {
    match comp {
        // No match: leave the line unchanged (R3.5).
        complete::Completion::None => {}
        // Unique match: replace the token with the full completion (R3.3).
        complete::Completion::Single(s) => {
            let new_line = format!("{}{}{}", prefix, dir_part, s);
            editor.set_line(&new_line);
            redraw_line(editor, shown);
        }
        // Several matches: extend to the longest common prefix, then list the
        // candidates and redraw the prompt + buffer (R3.4).
        complete::Completion::Multiple { lcp, candidates } => {
            let new_line = format!("{}{}{}", prefix, dir_part, lcp);
            editor.set_line(&new_line);
            list_candidates(editor, shown, &candidates);
        }
    }
}

/// Main shell loop.
///
/// Prints the welcome banner once (R10.1–R10.3), then runs one `LineEditor` per
/// prompt. A single `Decoder` and `History` persist across the whole session so
/// Shift/extended-prefix state and recall survive between prompts.
pub fn shell_main() -> ! {
    // Welcome banner — named OS + `help` hint, on both consoles, before the
    // first prompt with no perceptible delay (R10.1, R10.2, R10.3).
    crate::kprintln!();
    crate::kprintln!("========================================");
    crate::kprintln!("   Welcome to pagh OS Shell!");
    crate::kprintln!("========================================");
    crate::kprintln!("Type 'help' for available commands");
    crate::kprintln!();

    crate::fb_println!();
    crate::fb_println!("========================================");
    crate::fb_println!("   Welcome to pagh OS Shell!");
    crate::fb_println!("========================================");
    crate::fb_println!("Type 'help' for available commands");
    crate::fb_println!();

    // Session-long state: the decoder keeps Shift/0xE0-prefix state across
    // prompts; history accumulates recalled lines.
    let mut decoder = keys::Decoder::new();
    let mut history = history::History::new();

    loop {
        // CWD-aware prompt, e.g. `pagh:/> ` (R5.1–R5.3).
        render::prompt(&path::cwd());

        // Fresh editor per prompt; `shown` tracks rendered input width.
        let mut editor = editor::LineEditor::new();
        let mut shown: usize = 0;

        // Read keys until Enter.
        loop {
            // Idle on halt until an interrupt delivers a scancode (R11.5).
            crate::arch::cpu::halt();

            let scancode = match try_read_scancode() {
                Some(s) => s,
                None => continue,
            };

            // Unsupported / mid-prefix / break codes decode to None and are
            // ignored without panicking (R11.2).
            let event = match decoder.feed(scancode) {
                Some(e) => e,
                None => continue,
            };

            match event {
                keys::KeyEvent::Char(c) => {
                    editor.insert(c);
                    redraw_line(&editor, &mut shown);
                }
                keys::KeyEvent::Backspace => {
                    editor.delete_back();
                    redraw_line(&editor, &mut shown);
                }
                keys::KeyEvent::Delete => {
                    editor.delete_fwd();
                    redraw_line(&editor, &mut shown);
                }
                // Cursor moves update the logical cursor only; the visible caret
                // stays at end-of-line (see redraw_line LIMITATION note).
                keys::KeyEvent::Left => {
                    editor.move_left();
                }
                keys::KeyEvent::Right => {
                    editor.move_right();
                }
                keys::KeyEvent::Home => {
                    editor.move_home();
                }
                keys::KeyEvent::End => {
                    editor.move_end();
                }
                // Up: recall an older entry, stashing the live line (R2.2).
                keys::KeyEvent::Up => {
                    if let Some(line) = history.recall_prev(editor.buffer()) {
                        editor.set_line(line);
                        redraw_line(&editor, &mut shown);
                    }
                }
                // Down: recall a newer entry, or restore the stashed live line
                // when stepping past the newest (R2.3).
                keys::KeyEvent::Down => {
                    match history.recall_next() {
                        Some(line) => editor.set_line(line),
                        None => editor.set_line(history.saved_line()),
                    }
                    redraw_line(&editor, &mut shown);
                }
                keys::KeyEvent::Tab => {
                    handle_tab(&mut editor, &mut shown);
                }
                // Enter: finish the line. Non-empty lines are recorded (with
                // dedup) and dispatched; navigation is always reset.
                keys::KeyEvent::Enter => {
                    crate::kprintln!();
                    crate::fb_println!();
                    if editor.is_empty() {
                        history.reset_nav();
                    } else {
                        history.push(editor.buffer());
                        execute_command(editor.buffer());
                    }
                    break;
                }
            }
        }
    }
}

/// Try to read a scancode from the keyboard.
fn try_read_scancode() -> Option<u8> {
    crate::drivers::get_char("keyboard").and_then(|kbd| kbd.read_char())
}
