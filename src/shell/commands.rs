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

/// Copy all bytes from `src` to `dst` in chunks, capping total bytes so an
/// endless device cannot spin forever. Returns the number of bytes copied.
fn copy_bytes(
    src: &alloc::sync::Arc<dyn crate::vfs::VfsNode>,
    dst: &alloc::sync::Arc<dyn crate::vfs::VfsNode>,
) -> Result<u64, crate::vfs::VfsError> {
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let mut buf = [0u8; 512];
    let mut offset: u64 = 0;
    loop {
        if offset >= MAX_BYTES {
            break;
        }
        let n = src.read(offset, &mut buf)?;
        if n == 0 {
            break;
        }
        dst.write(offset, &buf[..n])?;
        offset += n as u64;
    }
    dst.sync();
    Ok(offset)
}

/// `cp <src> <dst>`: copy a file. If `dst` is an existing directory, the source
/// file is copied into it under its original name.
pub(super) fn cmd_cp(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("cp: usage: cp <src> <dst>");
        return;
    }
    let src = resolve_arg(args[0]);
    let dst = resolve_arg(args[1]);

    let src_node = match crate::vfs::lookup_path(&src) {
        Ok(n) => n,
        Err(_) => {
            shell_println(&alloc::format!("cp: {}: not found", src));
            return;
        }
    };
    if src_node.is_directory() {
        shell_println(&alloc::format!("cp: {}: is a directory (not copied)", src));
        return;
    }

    // Resolve the destination: into a directory (keep the source leaf name) or
    // to an explicit file path (create in its parent if needed).
    let dst_node = match crate::vfs::lookup_path(&dst) {
        Ok(node) if node.is_directory() => {
            let leaf = match split_path(&src) {
                Some((_, l)) => l,
                None => {
                    shell_println("cp: invalid source path");
                    return;
                }
            };
            match node.create_file(leaf) {
                Ok(n) => n,
                Err(crate::vfs::VfsError::AlreadyExists) => match node.lookup(leaf) {
                    Ok(n) => n,
                    Err(e) => {
                        shell_println(&alloc::format!("cp: {}: {:?}", dst, e));
                        return;
                    }
                },
                Err(e) => {
                    shell_println(&alloc::format!("cp: {}: {:?}", dst, e));
                    return;
                }
            }
        }
        Ok(node) => node, // overwrite existing file
        Err(_) => match split_path(&dst) {
            Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
                Ok(dir) => match dir.create_file(leaf) {
                    Ok(n) => n,
                    Err(e) => {
                        shell_println(&alloc::format!("cp: {}: {:?}", dst, e));
                        return;
                    }
                },
                Err(_) => {
                    shell_println(&alloc::format!("cp: {}: parent not found", dst));
                    return;
                }
            },
            None => {
                shell_println(&alloc::format!("cp: {}: invalid path", dst));
                return;
            }
        },
    };

    match copy_bytes(&src_node, &dst_node) {
        Ok(n) => shell_println(&alloc::format!("cp: copied {} bytes", n)),
        Err(e) => shell_println(&alloc::format!("cp: copy failed: {:?}", e)),
    }
}

/// `mv <src> <dst>`: move/rename a file (copy then remove the source).
pub(super) fn cmd_mv(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("mv: usage: mv <src> <dst>");
        return;
    }
    let src = resolve_arg(args[0]);

    let src_node = match crate::vfs::lookup_path(&src) {
        Ok(n) => n,
        Err(_) => {
            shell_println(&alloc::format!("mv: {}: not found", src));
            return;
        }
    };
    if src_node.is_directory() {
        shell_println(&alloc::format!("mv: {}: is a directory (not moved)", src));
        return;
    }

    // Reuse cp for the copy half, then unlink the source.
    cmd_cp(_ctx, args);

    match split_path(&src) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => {
                if let Err(e) = dir.remove(leaf) {
                    shell_println(&alloc::format!("mv: remove {}: {:?}", src, e));
                }
            }
            Err(_) => shell_println(&alloc::format!("mv: {}: parent not found", src)),
        },
        None => shell_println(&alloc::format!("mv: {}: invalid path", src)),
    }
}

/// `stat <path>`: print a file/directory's name, type, and size.
pub(super) fn cmd_stat(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("stat: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            let kind = if node.is_directory() { "directory" } else { "file" };
            shell_println(&alloc::format!("  path: {}", path));
            shell_println(&alloc::format!("  name: {}", node.name()));
            shell_println(&alloc::format!("  type: {}", kind));
            shell_println(&alloc::format!("  size: {} bytes", node.size()));
            if node.is_directory() {
                if let Ok(children) = node.readdir() {
                    shell_println(&alloc::format!("  entries: {}", children.len()));
                }
            }
        }
        Err(_) => shell_println(&alloc::format!("stat: {}: not found", path)),
    }
}

/// `sleep <seconds>`: block the shell for the given whole number of seconds,
/// halting between timer ticks instead of busy-spinning.
pub(super) fn cmd_sleep(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("sleep: usage: sleep <seconds>");
        return;
    }
    match args[0].parse::<u64>() {
        Ok(secs) => crate::task::scheduler::sleep_ticks(secs * 100),
        Err(_) => shell_println(&alloc::format!("sleep: invalid number '{}'", args[0])),
    }
}

/// `paint`: launch the framebuffer drawing application (mouse + keyboard).
pub(super) fn cmd_paint(_ctx: &mut ShellCtx, _args: &[&str]) {
    crate::kprintln!("paint: launching... (Esc or 'q' to quit)");
    crate::kprintln!(
        "paint keys: p=pencil e=eraser l=line r=rect f=fillrect c=circle d=disc b=bucket i=picker"
    );
    crate::kprintln!("paint keys: 1-0=color [ ]=brush u=undo x=clear s=save g=load q=quit");
    super::paint::run();
}

/// `pkg <host> <path> [port]`: download a `.deb` over HTTP and install its files
/// onto the mounted ext2 filesystem under `/mnt`.
///
/// Runs the full Package_Fetcher -> Deb_Parser -> Tar_Reader -> Package_Installer
/// pipeline: open a TCP connection to `host:port` (port defaults to 80), `GET` the
/// `.deb`, parse the `ar` container, decompress the `data.tar` (gzip only — `xz`/
/// `zstd` Debian packages are rejected with a clear message), and write every safe
/// regular file onto ext2. Networking must be up (see `ifconfig`).
///
/// Example:
///   pkg 10.0.2.2 /pool/main/h/hello/hello-static_2.10.gz.deb
pub(super) fn cmd_pkg(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("usage: pkg <host> <path> [port]");
        shell_println("  downloads a .deb over HTTP and installs it under /mnt");
        shell_println("  note: only gzip-compressed .deb data is supported (not xz/zstd)");
        shell_println("  tip: for name-based installs with dependency resolution, use `apt`");
        return;
    }
    let host = args[0];
    let path = args[1];
    let port: u16 = if args.len() >= 3 {
        match args[2].parse() {
            Ok(p) => p,
            Err(_) => {
                shell_println(&alloc::format!("pkg: invalid port '{}'", args[2]));
                return;
            }
        }
    } else {
        80
    };

    shell_println(&alloc::format!("pkg: downloading http://{}:{}{} ...", host, port, path));
    let bytes = match crate::net::http_fetch::fetch_deb(host, port, path) {
        Ok(b) => b.0,
        Err(e) => {
            shell_println(&alloc::format!("pkg: download failed: {:?}", e));
            return;
        }
    };
    shell_println(&alloc::format!("pkg: downloaded {} bytes", bytes.len()));

    // Parse the .deb ar container and locate its members.
    let members = match crate::pkg::deb::parse_ar(&bytes) {
        Ok(m) => m,
        Err(e) => {
            shell_println(&alloc::format!("pkg: not a valid .deb: {:?}", e));
            return;
        }
    };
    let deb = match crate::pkg::deb::locate_members(&members) {
        Ok(d) => d,
        Err(e) => {
            shell_println(&alloc::format!("pkg: malformed .deb: {:?}", e));
            return;
        }
    };

    // Decompress the data.tar member (gzip, xz, or zstd).
    let comp = match crate::pkg::deb::compression_of(deb.data.name) {
        Ok(c) => c,
        Err(e) => {
            shell_println(&alloc::format!("pkg: {:?}", e));
            return;
        }
    };
    let tar = match crate::pkg::deb::decompress_data(&deb.data, comp) {
        Ok(t) => t,
        Err(e) => {
            shell_println(&alloc::format!(
                "pkg: cannot decompress {} ({:?}); supported: gzip/xz/zstd .deb",
                deb.data.name, e
            ));
            return;
        }
    };

    // Enumerate and install the regular files onto ext2 under /mnt.
    let entries = match crate::pkg::tar::read_tar(&tar) {
        Ok(e) => e,
        Err(e) => {
            shell_println(&alloc::format!("pkg: bad data.tar: {:?}", e));
            return;
        }
    };
    match crate::pkg::install_fs::install_data_tar(&entries, "/mnt") {
        Ok(n) => {
            shell_println(&alloc::format!("pkg: installed {} files under /mnt", n));
            if let Ok(node) = crate::vfs::lookup_path("/mnt") {
                node.sync();
            }
            shell_println("pkg: done. Run an installed binary with: lxrun /mnt/<path>");
        }
        Err(e) => shell_println(&alloc::format!("pkg: install failed: {:?}", e)),
    }
}

/// `lxrun <path> [args...]`: run an installed statically-linked Linux binary as a
/// ring-3 `Compat_Process`.
///
/// Reads the ELF at `path` from ext2, loads it through the Linux ELF loader, builds
/// a System V initial stack with the supplied arguments, and enqueues it on the
/// scheduler. Only static `ET_EXEC` / static-PIE `ET_DYN` binaries are supported
/// (dynamically-linked binaries are rejected). Like `exec`, this runs with
/// interrupts disabled across the brief CR3 switch the loader/stack mapper perform.
pub(super) fn cmd_lxrun(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: lxrun <path> [args...]");
        return;
    }
    let path = resolve_arg(args[0]);
    // argv[0] is the program name as typed; remaining tokens follow.
    let argv: alloc::vec::Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();

    let result = crate::arch::cpu::without_interrupts(|| {
        crate::task::process::run_linux_binary(&path, &argv, &[])
    });
    match result {
        Ok(pid) => shell_println(&alloc::format!("lxrun: started Compat_Process pid {}", pid)),
        Err(e) => {
            let detail = match e {
                crate::task::process::RunError::ArgsTooLarge => "argument list too large",
                crate::task::process::RunError::NotFound => "file not found or unreadable",
                crate::task::process::RunError::LoadFailed(c) => c,
                crate::task::process::RunError::StackFailed => "initial stack construction failed",
            };
            shell_println(&alloc::format!("lxrun: {}: {}", path, detail));
        }
    }
}

/// `apt <subcommand> ...`: the name-driven package manager.
///
/// Unlike `pkg` (which takes a raw host/path/port), `apt` works by *package
/// name* against a downloaded repository index, resolving dependencies
/// automatically:
///
///   * `apt update`                  — download + parse the `Packages` index.
///   * `apt install <name> [name2…]` — resolve deps and install each package.
///   * `apt show <name>`             — print a package's index metadata.
///   * `apt list [substr]`           — list (matching) available package names.
///   * `apt setmirror <host> [base]` — point apt at a different mirror.
///
/// Transport: the default mirror uses HTTPS (TLS 1.3). NOTE: certificate
/// verification is NOT yet implemented, so HTTPS is encrypted but INSECURE
/// (unauthenticated / MITM-able). `setmirror` accepts an `http://`/`https://`
/// scheme prefix to switch transports.
///
/// The index lives only in RAM (the 64 MiB disk is too small to cache it), so
/// `apt update` must be re-run each boot before `install`/`show`/`list`.
pub(super) fn cmd_apt(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        apt_usage();
        return;
    }
    match args[0] {
        "update" => cmd_apt_update(),
        "install" => cmd_apt_install(&args[1..]),
        "show" => cmd_apt_show(&args[1..]),
        "list" => cmd_apt_list(&args[1..]),
        "setmirror" => cmd_apt_setmirror(&args[1..]),
        other => {
            shell_println(&alloc::format!("apt: unknown subcommand '{}'", other));
            apt_usage();
        }
    }
}

/// Print the `apt` usage summary.
fn apt_usage() {
    shell_println("usage: apt <command> [args]");
    shell_println("  update                  download & parse the package index");
    shell_println("  install <name> [name...]  install package(s) by name + deps");
    shell_println("  show <name>             show a package's metadata");
    shell_println("  list [substr]           list available package names");
    shell_println("  setmirror [scheme://]<host> [base]  set the mirror");
    shell_println("                          e.g. setmirror https://deb.debian.org /debian");
    shell_println("  transport: HTTPS (TLS 1.3) by default. NOTE: certificate verification");
    shell_println("             NOT yet implemented -- HTTPS is encrypted but INSECURE.");
}

/// `apt update`: refresh the in-RAM package index.
fn cmd_apt_update() {
    let cfg = crate::pkg::apt::config();
    if cfg.tls {
        shell_println("apt: NOTE HTTPS is INSECURE (TLS 1.3, certificate verification NOT yet implemented)");
    }
    shell_println(&alloc::format!(
        "apt: updating from {}://{}:{}{} ({}/{}/{}) ...",
        cfg.scheme(), cfg.host, cfg.port, cfg.base, cfg.suite, cfg.component, cfg.arch
    ));
    match crate::pkg::apt::update() {
        Ok(n) => shell_println(&alloc::format!("apt: {} packages available", n)),
        Err(e) => shell_println(&alloc::format!("apt: update failed: {}", e.message())),
    }
}

/// `apt install <name> [name2 …]`: resolve and install each named package.
fn cmd_apt_install(names: &[&str]) {
    if names.is_empty() {
        shell_println("usage: apt install <name> [name2 ...]");
        return;
    }
    for name in names {
        shell_println(&alloc::format!("apt: installing {} ...", name));
        match crate::pkg::apt::install(name) {
            Ok(installed) => {
                if installed.is_empty() {
                    shell_println(&alloc::format!(
                        "apt: {} is already installed (nothing to do)",
                        name
                    ));
                } else {
                    shell_println(&alloc::format!("Installed: {}", installed.join(" ")));
                }
            }
            Err(e) => shell_println(&alloc::format!("apt: {}: {}", name, e.message())),
        }
    }
    shell_println("apt: run an installed static binary with: lxrun /mnt/<path>");
}

/// `apt show <name>`: print a package's index metadata.
fn cmd_apt_show(args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: apt show <name>");
        return;
    }
    let name = args[0];
    match crate::pkg::apt::show(name) {
        Some(s) => {
            shell_println(&alloc::format!("Package: {}", s.package));
            shell_println(&alloc::format!("Version: {}", s.version));
            shell_println(&alloc::format!("Architecture: {}", s.arch));
            shell_println(&alloc::format!("Filename: {}", s.filename));
            let depends = if s.depends.is_empty() {
                String::from("(none)")
            } else {
                s.depends.join(", ")
            };
            shell_println(&alloc::format!("Depends: {}", depends));
            shell_println(&alloc::format!("Size: {} bytes", s.size));
        }
        None => {
            if crate::pkg::apt::has_index() {
                shell_println(&alloc::format!("apt: no such package '{}'", name));
            } else {
                shell_println("apt: no package index - run `apt update` first");
            }
        }
    }
}

/// `apt list [substr]`: list (matching) available package names, capped.
fn cmd_apt_list(args: &[&str]) {
    let filter = args.first().copied();
    if !crate::pkg::apt::has_index() {
        shell_println("apt: no package index - run `apt update` first");
        return;
    }
    let names = crate::pkg::apt::list(filter);
    if names.is_empty() {
        shell_println("apt: no matching packages");
        return;
    }
    // Cap output so a full index does not flood the console.
    const MAX_LINES: usize = 100;
    for name in names.iter().take(MAX_LINES) {
        shell_println(name);
    }
    if names.len() > MAX_LINES {
        shell_println(&alloc::format!(
            "... ({} more; narrow with `apt list <substr>`)",
            names.len() - MAX_LINES
        ));
    }
}

/// `apt setmirror <host> [basepath]`: change the active mirror.
fn cmd_apt_setmirror(args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: apt setmirror <host> [basepath]");
        return;
    }
    let host = args[0];
    let base = args.get(1).copied();
    crate::pkg::apt::set_mirror(host, base);
    let cfg = crate::pkg::apt::config();
    shell_println(&alloc::format!(
        "apt: mirror set to {}://{}:{}{}",
        cfg.scheme(), cfg.host, cfg.port, cfg.base
    ));
    if cfg.tls {
        shell_println("apt: transport is HTTPS (TLS 1.3) -- INSECURE: certificate verification NOT yet implemented");
    }
}
