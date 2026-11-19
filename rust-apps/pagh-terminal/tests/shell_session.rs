//! End-to-end contract of the terminal's shell session: the real binary in
//! `--shell-child` mode, driven through two pipes exactly the way the window
//! drives it.
//!
//! This is the same executable guest pagh runs (`pagh-terminal`), so the test
//! proves the spawn/pipes/echo/execute path that the QEMU run exercises, not a
//! mock of it.

use std::process::Command;
use std::time::{Duration, Instant};

use pagh_terminal::session::ShellSession;

fn spawn_shell() -> ShellSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pagh-terminal"));
    command.arg("--shell-child");
    ShellSession::spawn(command).expect("spawn --shell-child")
}

/// Read until `needle` shows up, returning everything read so far.
fn read_until(session: &mut ShellSession, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut text = Vec::new();
    while Instant::now() < deadline {
        if let Some(bytes) = session.try_read() {
            text.extend_from_slice(&bytes);
            if String::from_utf8_lossy(&text).contains(needle) {
                return String::from_utf8_lossy(&text).into_owned();
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "no {needle:?} within {timeout:?}; output so far: {:?}",
        String::from_utf8_lossy(&text)
    );
}

#[test]
fn shell_child_has_a_prompt_and_runs_help() {
    let mut shell = spawn_shell();
    // The very first output is the prompt — no boot log, no VT mirror.
    let prompt = read_until(&mut shell, "pagh:", Duration::from_secs(5));
    assert!(
        prompt.trim_end().ends_with('>'),
        "unexpected prompt: {prompt:?}"
    );

    shell.write_input(b"help\r").expect("write help");
    let text = read_until(&mut shell, "built-in commands", Duration::from_secs(5));
    assert!(text.contains("ls [DIR]"), "help must list ls: {text:?}");
    assert!(
        text.contains("not available here"),
        "help must name the kernel-shell gap: {text:?}"
    );
}

#[test]
fn shell_child_runs_ls_and_echo_on_the_typed_line() {
    let dir = std::env::temp_dir().join(format!("pagh-term-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("shown.txt"), b"hello").expect("write file");

    let mut shell = spawn_shell();
    read_until(&mut shell, "pagh:", Duration::from_secs(5));

    shell
        .write_input(format!("ls {}\r", dir.display()).as_bytes())
        .expect("write ls");
    let text = read_until(&mut shell, "shown.txt", Duration::from_secs(5));
    assert!(
        text.contains("shown.txt"),
        "ls did not list the file: {text:?}"
    );

    shell.write_input(b"echo session-ok\r").expect("write echo");
    let text = read_until(&mut shell, "session-ok", Duration::from_secs(5));
    // The typed line is echoed by the shell itself (a pipe is not a terminal).
    assert!(
        text.matches("session-ok").count() >= 2,
        "echo must repeat the input: {text:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn shell_child_edits_before_enter() {
    let mut shell = spawn_shell();
    read_until(&mut shell, "pagh:", Duration::from_secs(5));

    // Type the wrong character, Backspace (0x7f), the right one, Enter: the
    // window sends raw VT bytes, so the shell must own line editing.
    shell
        .write_input(b"heX\x7flp\r")
        .expect("write edited help");
    let text = read_until(&mut shell, "built-in commands", Duration::from_secs(5));
    assert!(
        text.contains("built-in commands"),
        "edited line did not run help: {text:?}"
    );
}

#[test]
fn shell_child_exits_on_exit_command() {
    let mut shell = spawn_shell();
    read_until(&mut shell, "pagh:", Duration::from_secs(5));
    shell.write_input(b"exit 3\r").expect("write exit");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match shell.try_wait() {
            Ok(Some(status)) => {
                assert_eq!(status.code(), Some(3), "exit status must reach the parent");
                break;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => panic!("shell did not exit after `exit 3`"),
            Err(error) => panic!("try_wait failed: {error}"),
        }
    }
}
