//! Idempotent first-boot filesystem provisioning for pagh.

use crate::vfs::{VfsError, VfsNode};
use alloc::sync::Arc;

const RELEASE: &[u8] = b"NAME=pagh OS\nID=pagh\nVERSION=0.2-dev\nARCH=x86_64\n";
const MOTD: &[u8] =
    b"pagh OS 0.2-dev\nType 'help' to list commands. Network apt and nano+ are available.\n";
const README: &[u8] = b"Welcome to pagh!\n\nTry:\n  help\n  nano --settings\n  nano /mnt/home/user/notes.txt\n  cargo run /mnt/examples/hello\n  apt update\n";
const CARGO: &[u8] = b"[package]\nname = \"pagh-hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const MAIN_RS: &[u8] = b"fn main() {\n    let values = [8, 16, 32];\n    println!(\"Hello from pagh mini-Rust! sum = {}\", values.iter().sum());\n}\n";

fn ensure_dir(path: &str) -> Result<Arc<dyn VfsNode>, VfsError> {
    let mut node = crate::vfs::lookup_path("/")?;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        node = match node.lookup(part) {
            Ok(child) if child.is_directory() => child,
            Ok(_) => return Err(VfsError::InvalidArgument),
            Err(VfsError::NotFound) => node.create_dir(part)?,
            Err(error) => return Err(error),
        };
    }
    Ok(node)
}

fn write_once(path: &str, data: &[u8]) -> Result<(), VfsError> {
    if crate::vfs::lookup_path(path).is_ok() {
        return Ok(());
    }
    let split = path.rfind('/').ok_or(VfsError::InvalidArgument)?;
    let parent = if split == 0 { "/" } else { &path[..split] };
    let dir = ensure_dir(parent)?;
    let file = dir.create_file(&path[split + 1..])?;
    if file.write(0, data)? != data.len() {
        return Err(VfsError::IoError);
    }
    file.sync();
    Ok(())
}

/// Install release metadata, a home skeleton and examples on the first boot.
/// Existing files are never overwritten.
pub fn seed() {
    if crate::vfs::lookup_path("/mnt/etc/pagh-release").is_ok() {
        return;
    }
    let result = (|| {
        ensure_dir("/mnt/etc")?;
        ensure_dir("/mnt/home/user")?;
        ensure_dir("/mnt/usr/share/pagh")?;
        ensure_dir("/mnt/examples/hello/src")?;
        write_once("/mnt/etc/pagh-release", RELEASE)?;
        write_once("/mnt/etc/motd", MOTD)?;
        write_once("/mnt/home/user/README.txt", README)?;
        write_once(
            "/mnt/usr/share/pagh/LICENSE-NOTICE.txt",
            b"pagh kernel and bundled development environment: MIT.\n",
        )?;
        write_once("/mnt/examples/hello/Cargo.toml", CARGO)?;
        write_once("/mnt/examples/hello/src/main.rs", MAIN_RS)?;
        Ok::<(), VfsError>(())
    })();
    match result {
        Ok(()) => crate::info!("pagh first-boot files installed"),
        Err(error) => crate::warn!("pagh first-boot provisioning failed: {:?}", error),
    }
}

// ─── STAGE-13.8: first-boot base userland (glibc + python3) ──────────────────

/// True when any glibc CPython is present under /mnt/usr/bin.
fn python_installed() -> bool {
    let Ok(dir) = crate::vfs::lookup_path("/mnt/usr/bin") else {
        return false;
    };
    let Ok(entries) = dir.readdir() else {
        return false;
    };
    entries.iter().any(|n| n.name().starts_with("python3"))
}

/// Download and install the base userland (glibc + python3 with its full
/// stdlib) through the apt subsystem when a freshly formatted disk has none.
/// Runs on a dedicated kernel thread spawned late in boot; waits for DHCP by
/// retrying `apt update`. Idempotent: exits immediately when python is
/// already on disk, so provisioned images boot with zero overhead.
/// Milestone visible on BOTH consoles while the framebuffer mirror is paused:
/// `info!` covers serial; the explicit `fb_println!` covers the screen.
fn announce(msg: &str) {
    crate::info!("{}", msg);
    crate::fb_println!("[provision] {}", msg);
}

pub fn ensure_base_packages_thread() {
    if crate::vfs::lookup_path("/mnt").is_err() {
        return; // no disk mounted
    }
    if python_installed() {
        return;
    }
    // STAGE-13.8: keep the interactive console clean -- all apt chatter
    // (download progress, index parsing, installer output) goes to serial
    // only; the framebuffer sees just the milestones below. The full Debian
    // index parse alone takes a couple of minutes of quiet CPU time.
    crate::log::set_fb_mirror_paused(true);
    announce("provision: no python on disk; installing glibc + python3 in the background (progress on serial; the shell stays usable; this takes a few minutes)");
    let mut index_ready = crate::pkg::apt::has_index();
    if !index_ready {
        // Let boot + DHCP settle before touching the network at all.
        crate::task::scheduler::sleep_ticks(1000); // ~10 s at 100 Hz
        let mut fatal = false;
        for attempt in 1..=6u32 {
            match crate::pkg::apt::update() {
                Ok(n) => {
                    crate::info!("provision: apt index ready ({} packages)", n);
                    index_ready = true;
                    break;
                }
                // (failure arms below keep their serial-only warns)
                // Network not up yet / transfer hiccup: worth waiting for.
                Err(e @ crate::pkg::apt::AptOpError::NoNetwork)
                | Err(e @ crate::pkg::apt::AptOpError::Download { .. }) => {
                    crate::warn!("provision: apt update ({}/6): {}", attempt, e.message());
                    crate::task::scheduler::sleep_ticks(500);
                }
                // Anything else (index too large, disabled build, parse bug)
                // is deterministic: retrying only re-downloads megabytes and
                // spams the console. Bail out once, quietly.
                Err(e) => {
                    crate::warn!("provision: apt update failed: {}", e.message());
                    crate::fb_println!("[provision] apt update failed: {}", e.message());
                    fatal = true;
                    break;
                }
            }
        }
        if !index_ready {
            if fatal {
                announce("provision: giving up; fix the cause, then run 'apt update' + 'apt install python3' from the shell");
            } else {
                announce("provision: network unavailable; run 'apt update' + 'apt install python3' later");
            }
            crate::log::set_fb_mirror_paused(false);
            return;
        }
    }
    // The resolver pulls the whole dependency chain (libc6, libpython3.x,
    // libpython3.x-stdlib, ...) exactly like a shell 'apt install python3'.
    match crate::pkg::apt::install("python3") {
        Ok(pkgs) => {
            crate::info!(
                "provision: python3 installed ({} package(s) total)",
                pkgs.len()
            );
        }
        Err(e) => {
            announce("provision: install python3 failed (details on serial); run 'apt install python3' from the shell to retry");
            crate::warn!("provision: install python3 failed: {}", e.message());
            crate::log::set_fb_mirror_paused(false);
            return;
        }
    }
    crate::log::set_fb_mirror_paused(false);
    if python_installed() {
        announce("provision: base userland ready — type 'python'");
    }
}
