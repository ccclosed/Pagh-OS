//! Idempotent first-boot filesystem provisioning for pagh.

use crate::vfs::{VfsError, VfsNode};
use alloc::sync::Arc;

// OS version is derived from the crate version (Cargo.toml) so the two
// cannot drift apart.
const RELEASE: &[u8] = concat!(
    "NAME=pagh OS\nID=pagh\nVERSION=",
    env!("CARGO_PKG_VERSION"),
    "\nARCH=x86_64\n"
)
.as_bytes();
const MOTD: &[u8] = concat!(
    "pagh OS ",
    env!("CARGO_PKG_VERSION"),
    "\nType 'help' to list commands. Network apt and nano+ are available.\n"
)
.as_bytes();
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
        // glibc's resolver reads these before any getaddrinfo call works;
        // 10.0.2.3 is QEMU slirp's built-in DNS forwarder.
        write_once(
            "/mnt/etc/resolv.conf",
            b"nameserver 10.0.2.3\noptions timeout:1 attempts:2\n",
        )?;
        write_once("/mnt/etc/hosts", b"127.0.0.1 localhost\n::1 localhost\n");
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
