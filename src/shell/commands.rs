//! Command handlers (`help`, `clear`, `echo`, ...) built on the registry.
//!
//! Each handler has the signature `fn(ctx: &mut ShellCtx, args: &[&str])`,
//! where `args` are the tokens that follow the command name (so for the line
//! `write /mnt/f hi`, the `write` handler receives `["/mnt/f", "hi"]`). The
//! handlers are registered in [`super::registry::COMMANDS`] and invoked by the
//! dispatcher in `mod.rs` via [`super::registry::lookup`].
//!
//! Behavior is migrated verbatim from the previous hand-maintained
//! `execute_command` match: identical output text, error messages, and logic.
//! Paths are still used as absolute, exactly as before (CWD-relative
//! resolution lands in a later task). The dual-output helpers
//! ([`shell_println`] / [`shell_print_bytes`]) and the path/networking helpers
//! live here so handlers can share them.

use alloc::string::String;

use super::registry::ShellCtx;

/// Print a line to both serial and framebuffer, matching the shell's
/// dual-output convention.
pub(super) fn shell_println(text: &str) {
    crate::kprintln!("{}", text);
    crate::fb_println!("{}", text);
}

/// Print raw bytes as text to both serial and framebuffer. Valid UTF-8 is
/// printed as-is; invalid bytes are replaced so the routine stays panic-free.
pub(super) fn shell_print_bytes(bytes: &[u8]) {
    let mut start = 0;
    while start < bytes.len() {
        match core::str::from_utf8(&bytes[start..]) {
            Ok(s) => {
                crate::kprint!("{}", s);
                crate::fb_print!("{}", s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    // SAFETY: bytes[start..start+valid] is validated UTF-8.
                    let s = unsafe { core::str::from_utf8_unchecked(&bytes[start..start + valid]) };
                    crate::kprint!("{}", s);
                    crate::fb_print!("{}", s);
                }
                // Emit a replacement char for the invalid byte and skip it.
                crate::kprint!("\u{FFFD}");
                crate::fb_print!("\u{FFFD}");
                start += valid + 1;
            }
        }
    }
}

/// Resolve a user-supplied path argument against the shell's current working
/// directory, returning an absolute, normalized path.
///
/// Absolute inputs (those starting with `/`) ignore the CWD, so users typing
/// `/mnt/...` behave exactly as before; relative inputs and `.`/`..` are folded
/// against the CWD (R4.6, R9.5). All path-taking handlers route their user
/// argument through this so relative paths work everywhere.
fn resolve_arg(arg: &str) -> String {
    super::path::resolve(&super::path::cwd(), arg)
}

/// Split an absolute path into `(parent_path, leaf_name)`.
///
/// `"/mnt/foo"` -> `("/mnt", "foo")`, `"/mnt/a/b"` -> `("/mnt/a", "b")`,
/// `"/foo"` -> `("/", "foo")`. Returns `None` for paths with no leaf
/// (`"/"`, `""`, or a trailing-slash-only path).
fn split_path(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let idx = trimmed.rfind('/')?;
    let leaf = &trimmed[idx + 1..];
    if leaf.is_empty() {
        return None;
    }
    let parent = if idx == 0 { "/" } else { &trimmed[..idx] };
    Some((parent, leaf))
}

/// `help` (R6.2/6.3/6.4): with no argument, list every registered command and
/// its one-line description; with an argument, print that command's usage and
/// description, or a clear "no such command" message when unknown. Generated
/// entirely from the registry so the list never drifts from the table.
pub(super) fn cmd_help(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("Available commands:");
        for spec in super::registry::COMMANDS {
            shell_println(&alloc::format!("  {:8} - {}", spec.name, spec.description));
        }
    } else {
        let name = args[0];
        match super::registry::lookup(name) {
            Some(spec) => {
                shell_println(&alloc::format!("usage: {}", spec.usage));
                shell_println(spec.description);
            }
            None => {
                shell_println(&alloc::format!("help: no such command '{}'", name));
            }
        }
    }
}

/// `clear` (R9.3): clear both the serial scrollback and the framebuffer view.
pub(super) fn cmd_clear(_ctx: &mut ShellCtx, _args: &[&str]) {
    for _ in 0..50 {
        crate::kprintln!();
    }
    crate::drivers::framebuffer::clear_screen();
}

/// `echo` (R9.4): join the arguments with single spaces and print the result.
pub(super) fn cmd_echo(_ctx: &mut ShellCtx, args: &[&str]) {
    if !args.is_empty() {
        let text = args.join(" ");
        crate::kprintln!("{}", text);
        crate::fb_println!("{}", text);
    }
}

/// `uptime`: print the scheduler tick count and an approximate seconds value.
pub(super) fn cmd_uptime(_ctx: &mut ShellCtx, _args: &[&str]) {
    let ticks = crate::task::scheduler::ticks();
    crate::kprintln!("Ticks: {} (~{} sec)", ticks, ticks / 100);
    crate::fb_println!("Ticks: {} (~{} sec)", ticks, ticks / 100);
}

/// `pwd` (R4.2): print the absolute current working directory.
pub(super) fn cmd_pwd(_ctx: &mut ShellCtx, _args: &[&str]) {
    shell_println(&super::path::cwd());
}

/// `cd [path]`: change the shell's current working directory.
///
/// With no argument, reset the CWD to the home default `/` (R4.4). With a path,
/// resolve it against the current CWD, verify it names an existing directory,
/// and on success store it (R4.3). A missing path or a non-directory target
/// prints a clear error and leaves the CWD unchanged (R4.5).
pub(super) fn cmd_cd(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        // No argument: reset to the home default `/` (R4.4).
        super::path::set_cwd("/");
        return;
    }

    let target = super::path::resolve(&super::path::cwd(), args[0]);
    match crate::vfs::lookup_path(&target) {
        Ok(node) => {
            if node.is_directory() {
                super::path::set_cwd(&target);
            } else {
                super::render::error_line(&alloc::format!("cd: {}: not a directory", args[0]));
            }
        }
        Err(_) => {
            super::render::error_line(&alloc::format!("cd: {}: not found", args[0]));
        }
    }
}

/// `ls [path]`: list a directory's entries (directories shown with a trailing
/// `/`), or print a file's own name. Defaults to `/` when no path is given.
pub(super) fn cmd_ls(_ctx: &mut ShellCtx, args: &[&str]) {
    // No arg: list the current working directory (R9.2). With an arg, resolve
    // it against the CWD so relative paths and `.`/`..` work (R4.6).
    let path = if !args.is_empty() {
        resolve_arg(args[0])
    } else {
        super::path::cwd()
    };
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            if node.is_directory() {
                match node.readdir() {
                    Ok(children) => {
                        for child in children.iter() {
                            if child.is_directory() {
                                shell_println(&alloc::format!("{}/", child.name()));
                            } else {
                                shell_println(child.name());
                            }
                        }
                    }
                    Err(_) => {
                        shell_println(&alloc::format!("ls: {}: cannot read directory", path));
                    }
                }
            } else {
                // Non-directory: print just its name, like `ls file`.
                shell_println(node.name());
            }
        }
        Err(_) => {
            shell_println(&alloc::format!("ls: {}: not found", path));
        }
    }
}

/// `cat <path>`: print a file's contents, capping the total bytes read so an
/// endless device cannot spin forever.
pub(super) fn cmd_cat(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("cat: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            if node.is_directory() {
                shell_println(&alloc::format!("cat: {}: is a directory", path));
            } else {
                // Read in chunks, capping total bytes to avoid spinning forever
                // on endless devices.
                const MAX_BYTES: u64 = 64 * 1024;
                let mut offset: u64 = 0;
                let mut buf = [0u8; 256];
                loop {
                    if offset >= MAX_BYTES {
                        break;
                    }
                    match node.read(offset, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            shell_print_bytes(&buf[..n]);
                            offset += n as u64;
                        }
                        Err(_) => {
                            shell_println(&alloc::format!("cat: {}: read error", path));
                            break;
                        }
                    }
                }
            }
        }
        Err(_) => {
            shell_println(&alloc::format!("cat: {}: not found", path));
        }
    }
}

/// `mkdir <path>`: create a directory under an existing parent.
pub(super) fn cmd_mkdir(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("mkdir: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match split_path(&path) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => match dir.create_dir(leaf) {
                Ok(_) => {}
                Err(e) => shell_println(&alloc::format!("mkdir: {}: {:?}", path, e)),
            },
            Err(_) => shell_println(&alloc::format!("mkdir: {}: parent not found", path)),
        },
        None => shell_println(&alloc::format!("mkdir: {}: invalid path", path)),
    }
}

/// `touch <path>`: create an empty file (a no-op when it already exists).
pub(super) fn cmd_touch(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("touch: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match split_path(&path) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => match dir.create_file(leaf) {
                Ok(_) => {}
                Err(crate::vfs::VfsError::AlreadyExists) => {}
                Err(e) => shell_println(&alloc::format!("touch: {}: {:?}", path, e)),
            },
            Err(_) => shell_println(&alloc::format!("touch: {}: parent not found", path)),
        },
        None => shell_println(&alloc::format!("touch: {}: invalid path", path)),
    }
}

/// `write <path> <text>`: write text to a file, creating it if needed.
pub(super) fn cmd_write(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("write: usage: write <path> <text>");
        return;
    }
    let path = resolve_arg(args[0]);
    let text = args[1..].join(" ");
    // Open the file if it exists, otherwise create it (in its parent).
    let file = match crate::vfs::lookup_path(&path) {
        Ok(node) => Ok(node),
        Err(_) => match split_path(&path) {
            Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
                Ok(dir) => dir.create_file(leaf),
                Err(_) => Err(crate::vfs::VfsError::NotFound),
            },
            None => Err(crate::vfs::VfsError::InvalidArgument),
        },
    };
    match file {
        Ok(node) => {
            if node.is_directory() {
                shell_println(&alloc::format!("write: {}: is a directory", path));
            } else {
                match node.write(0, text.as_bytes()) {
                    Ok(n) => {
                        node.sync();
                        shell_println(&alloc::format!("write: wrote {} bytes to {}", n, path));
                    }
                    Err(e) => shell_println(&alloc::format!("write: {}: {:?}", path, e)),
                }
            }
        }
        Err(e) => shell_println(&alloc::format!("write: {}: {:?}", path, e)),
    }
}

/// `rm <path>`: remove a file or empty directory.
pub(super) fn cmd_rm(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("rm: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match split_path(&path) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => match dir.remove(leaf) {
                Ok(()) => {}
                Err(e) => shell_println(&alloc::format!("rm: {}: {:?}", path, e)),
            },
            Err(_) => shell_println(&alloc::format!("rm: {}: parent not found", path)),
        },
        None => shell_println(&alloc::format!("rm: {}: invalid path", path)),
    }
}

/// `sync`: flush the mounted filesystem.
pub(super) fn cmd_sync(_ctx: &mut ShellCtx, _args: &[&str]) {
    // Flush the mounted filesystem. Every mutation is already journaled at
    // commit time, so this is a no-op confirmation in v1.
    match crate::vfs::lookup_path("/mnt") {
        Ok(node) => {
            node.sync();
            shell_println("sync: ok");
        }
        Err(_) => shell_println("sync: /mnt not mounted"),
    }
}

/// `fscrash`: demonstrate journal replay + persistence on the real disk.
pub(super) fn cmd_fscrash(_ctx: &mut ShellCtx, _args: &[&str]) {
    fscrash_demo();
}

/// `pci`: enumerate and list PCI devices.
pub(super) fn cmd_pci(_ctx: &mut ShellCtx, _args: &[&str]) {
    let devices = crate::drivers::pci::enumerate();
    if devices.is_empty() {
        shell_println("pci: no devices found");
    } else {
        for dev in devices.iter() {
            let a = dev.address;
            let tag = if dev.is_virtio() { " [virtio]" } else { "" };
            shell_println(&alloc::format!(
                "{:02x}:{:02x}.{} {:04x}:{:04x} {:02x}:{:02x}{}",
                a.bus, a.device, a.function,
                dev.vendor_id, dev.device_id,
                dev.class, dev.subclass, tag
            ));
        }
    }
}

/// `exec`: run the embedded test user process.
pub(super) fn cmd_exec(_ctx: &mut ShellCtx, _args: &[&str]) {
    // create_user_process programs the TSS RSP0 and briefly switches CR3; run
    // it with interrupts disabled so the CR3 window isn't preempted on this
    // interruptible shell thread.
    let result = crate::arch::cpu::without_interrupts(
        || crate::task::process::spawn_test_user_process(),
    );
    match result {
        Ok(pid) => {
            shell_println(&alloc::format!("exec: started user process pid {}", pid));
        }
        Err(e) => {
            shell_println(&alloc::format!("exec: failed: {}", e));
        }
    }
}

/// `ifconfig`: show the network interface configuration.
pub(super) fn cmd_ifconfig(_ctx: &mut ShellCtx, _args: &[&str]) {
    match crate::net::ip_config() {
        Some(cfg) => {
            let m = cfg.mac.0;
            shell_println(&alloc::format!("eth0  inet {}", cfg.addr));
            shell_println(&alloc::format!("      gateway {}", cfg.gateway));
            shell_println(&alloc::format!(
                "      ether {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                m[0], m[1], m[2], m[3], m[4], m[5]
            ));
        }
        None => shell_println("ifconfig: no interface"),
    }
}

/// `nc <ip> <port> [text]` (R15.2, R15.3): open a TCP connection to the given
/// endpoint and echo data over it.
///
/// v1 simplification (documented): the shell is line-based, so rather than a
/// fully interactive session this connects, sends a single line (the trailing
/// `text` args, or a default test string when none are given), collects the
/// bytes echoed back within a short bounded poll window, prints them, and
/// closes. This exercises the guest TCP client path against an echo server.
pub(super) fn cmd_nc(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("usage: nc <ip> <port> [text]");
        return;
    }

    let ip = match parse_ipv4(args[0]) {
        Some(ip) => ip,
        None => {
            shell_println("nc: invalid IPv4 address");
            return;
        }
    };
    let port: u16 = match args[1].parse() {
        Ok(p) => p,
        Err(_) => {
            shell_println("nc: invalid port");
            return;
        }
    };

    // Payload: the remaining args joined, or a default test line.
    let payload: String = if args.len() > 2 {
        args[2..].join(" ")
    } else {
        String::from("pagh-nc-test")
    };

    let remote = smoltcp::wire::IpEndpoint::new(
        smoltcp::wire::IpAddress::Ipv4(ip),
        port,
    );

    shell_println(&alloc::format!(
        "nc: connecting to {}:{} ...",
        args[0], port
    ));

    match crate::net::nc_echo(remote, payload.as_bytes()) {
        crate::net::NcResult::Echoed(bytes) => {
            if bytes.is_empty() {
                shell_println("nc: connected, no data echoed back");
            } else {
                shell_println(&alloc::format!("nc: echoed {} bytes:", bytes.len()));
                shell_print_bytes(&bytes);
                crate::kprintln!();
                crate::fb_println!();
            }
        }
        crate::net::NcResult::Failed => {
            shell_println("nc: connection failed");
        }
    }
}

/// `selftest`: run the kernel self-test suite (output on serial).
pub(super) fn cmd_selftest(_ctx: &mut ShellCtx, _args: &[&str]) {
    // All registered routines are non-destructive (they restore PMM free
    // counts, heap, interrupt state, VFS, etc.), so it is safe to run them
    // interactively. Output goes over serial via kprintln!.
    shell_println("Running kernel self-test (output on serial)...");
    crate::test::run_all();
    shell_println("Self-test complete (see serial log).");
}

/// Parse a dotted-quad IPv4 address (e.g. `10.0.2.2`) into an `Ipv4Address`.
fn parse_ipv4(s: &str) -> Option<smoltcp::wire::Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in s.split('.') {
        if count >= 4 {
            return None;
        }
        octets[count] = part.parse().ok()?;
        count += 1;
    }
    if count != 4 {
        return None;
    }
    Some(smoltcp::wire::Ipv4Address::new(
        octets[0], octets[1], octets[2], octets[3],
    ))
}

/// `fscrash` demo (Task 5.3): demonstrate journal replay + persistence on the
/// real disk end-to-end.
///
/// Writes `/mnt/crashtest.txt` with known content through the journaled write
/// path, then re-mounts the ext2 filesystem from the same block device — which
/// runs `journal.recover()` — and reads the file back, asserting the committed
/// write survived. Prints PASS/FAIL.
///
/// A true mid-write power-cut cannot be injected on live hardware; the RAM-mock
/// property tests P10/P11 cover crash atomicity. This command demonstrates the
/// mount + recover + persist path end-to-end on the real device.
fn fscrash_demo() {
    use crate::fs::ext2::Ext2Fs;

    const CONTENT: &[u8] = b"pagh-fscrash-journal-replay-consistency-OK";

    let blk = match crate::drivers::get_block("virtio-blk0") {
        Some(b) => b,
        None => {
            shell_println("fscrash: no virtio-blk device");
            return;
        }
    };

    // 1. Mount and write the test file through the journaled path.
    let root = match Ext2Fs::mount(blk.clone()) {
        Ok(r) => r,
        Err(e) => {
            shell_println(&alloc::format!("fscrash: mount failed: {:?}", e));
            return;
        }
    };
    let file = match root.create_file("crashtest.txt") {
        Ok(f) => f,
        Err(crate::vfs::VfsError::AlreadyExists) => match root.lookup("crashtest.txt") {
            Ok(f) => f,
            Err(e) => {
                shell_println(&alloc::format!("fscrash: open failed: {:?}", e));
                return;
            }
        },
        Err(e) => {
            shell_println(&alloc::format!("fscrash: create failed: {:?}", e));
            return;
        }
    };
    if let Err(e) = file.write(0, CONTENT) {
        shell_println(&alloc::format!("fscrash: write failed: {:?}", e));
        return;
    }
    shell_println("fscrash: wrote /mnt/crashtest.txt (committed via journal)");

    // 2. Force a fresh remount; this runs journal recover() before building the
    //    root, simulating the post-crash mount path.
    shell_println("fscrash: remounting (triggers journal recover)...");
    let root2 = match Ext2Fs::mount(blk) {
        Ok(r) => r,
        Err(e) => {
            shell_println(&alloc::format!("fscrash: remount failed: {:?}", e));
            return;
        }
    };

    // 3. Read the file back and verify the content persisted.
    let node = match root2.lookup("crashtest.txt") {
        Ok(n) => n,
        Err(e) => {
            shell_println(&alloc::format!("fscrash: FAIL — file missing after remount: {:?}", e));
            return;
        }
    };
    let mut buf = [0u8; 64];
    match node.read(0, &mut buf) {
        Ok(n) if &buf[..n] == CONTENT => {
            shell_println(&alloc::format!(
                "fscrash: PASS — content survived remount/replay ({} bytes), filesystem consistent",
                n
            ));
        }
        Ok(n) => {
            shell_println(&alloc::format!("fscrash: FAIL — content mismatch after remount ({} bytes)", n));
        }
        Err(e) => shell_println(&alloc::format!("fscrash: FAIL — read-back error: {:?}", e)),
    }
}
