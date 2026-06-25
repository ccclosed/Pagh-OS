//! A tiny interactive serial shell over the ns16550 UART (polled RX).
//!
//! This is the seed's interactive entry point: after the boot-time milestone
//! demos it reads a line from the UART, echoes it, and runs a small built-in
//! command set. Input is polled at the ~100 Hz timer cadence (no PLIC yet);
//! interrupt-driven RX is a later step. The richer pagh shell (history, editing,
//! completion, VFS commands) is arch-independent and folds in once the ext2/VFS
//! layer is integrated.

use alloc::string::String;

/// Print the prompt (no newline).
fn prompt() {
    crate::kprint!("pagh-rv> ");
}

/// Run the interactive shell forever.
pub fn run() -> ! {
    crate::kprintln!();
    crate::kprintln!("==================================================");
    crate::kprintln!("  pagh-rv interactive shell (riscv64, serial)");
    crate::kprintln!("  commands: help  ticks  mem  info  clear");
    crate::kprintln!("==================================================");
    prompt();

    let mut line = String::new();
    loop {
        match crate::uart::getb() {
            Some(b'\r') | Some(b'\n') => {
                crate::kprintln!();
                exec(line.trim());
                line.clear();
                prompt();
            }
            // Backspace / DEL: erase one char on the terminal.
            Some(0x08) | Some(0x7f) => {
                if line.pop().is_some() {
                    crate::kprint!("\u{8} \u{8}");
                }
            }
            // Printable ASCII: echo and accumulate.
            Some(c @ 0x20..=0x7e) => {
                line.push(c as char);
                crate::kprint!("{}", c as char);
            }
            // Other control bytes: ignore.
            Some(_) => {}
            // Nothing pending: wait for the next tick so we don't busy-spin.
            None => {
                // SAFETY: `wfi` waits for the next (timer) interrupt.
                unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
            }
        }
    }
}

/// Execute one entered command line.
fn exec(cmd: &str) {
    let mut parts = cmd.split_whitespace();
    let c = parts.next().unwrap_or("");
    match c {
        "" => {}
        "help" => {
            crate::kprintln!("commands:");
            crate::kprintln!("  help                 - this list");
            crate::kprintln!("  ticks                - timer ticks since boot (~100 Hz)");
            crate::kprintln!("  mem                  - physical frame allocator stats");
            crate::kprintln!("  disk                 - virtio-blk capacity + sector 0 preview");
            crate::kprintln!("  net                  - DHCP lease (address + gateway)");
            crate::kprintln!("  ls                   - list files (ramfs)");
            crate::kprintln!("  write <name> <text>  - create/overwrite a file");
            crate::kprintln!("  cat <name>           - print a file");
            crate::kprintln!("  rm <name>            - remove a file");
            crate::kprintln!("  info                 - system / port info");
            crate::kprintln!("  clear                - clear the screen");
        }
        "ticks" => {
            let t = crate::timer::ticks();
            crate::kprintln!("{} ticks (~{} s)", t, t / 100);
        }
        "mem" => {
            let (free, total) = crate::pmm::stats();
            crate::kprintln!(
                "PMM: {} / {} frames free ({} MiB free of {} MiB)",
                free,
                total,
                free * crate::pmm::FRAME_SIZE / (1024 * 1024),
                total * crate::pmm::FRAME_SIZE / (1024 * 1024)
            );
        }
        "ls" => {
            let files = crate::ramfs::list();
            if files.is_empty() {
                crate::kprintln!("(no files)");
            } else {
                for (name, size) in files {
                    crate::kprintln!("{:>8}  {}", size, name);
                }
            }
        }
        "write" => match parts.next() {
            Some(name) => {
                let rest = collect_rest(parts);
                crate::ramfs::write(name, &rest);
                crate::kprintln!("wrote {} ({} bytes)", name, rest.len());
            }
            None => crate::kprintln!("usage: write <name> <text>"),
        },
        "cat" => match parts.next() {
            Some(name) => match crate::ramfs::read(name) {
                Some(content) => crate::kprintln!("{}", content),
                None => crate::kprintln!("no such file: {}", name),
            },
            None => crate::kprintln!("usage: cat <name>"),
        },
        "rm" => match parts.next() {
            Some(name) => {
                if crate::ramfs::remove(name) {
                    crate::kprintln!("removed {}", name);
                } else {
                    crate::kprintln!("no such file: {}", name);
                }
            }
            None => crate::kprintln!("usage: rm <name>"),
        },
        "disk" => match crate::blk::capacity() {
            Some(cap) => {
                crate::kprintln!("virtio-blk: {} sectors ({} MiB)", cap, cap * 512 / (1024 * 1024));
                let mut buf = [0u8; 512];
                if crate::blk::read_sector(0, &mut buf) {
                    crate::kprint!("sector 0:");
                    for b in &buf[..16] {
                        crate::kprint!(" {:02x}", b);
                    }
                    crate::kprintln!();
                }
            }
            None => crate::kprintln!("no virtio-blk device"),
        },
        "net" => match crate::net::ip_info() {
            Some((addr, gw)) => {
                crate::kprintln!("address: {}", addr);
                match gw {
                    Some(g) => crate::kprintln!("gateway: {}", g),
                    None => crate::kprintln!("gateway: (none)"),
                }
            }
            None => crate::kprintln!("no DHCP lease"),
        },
        "info" => {
            crate::kprintln!("pagh-rv: riscv64gc, S-mode under OpenSBI, Sv39 paging");
            crate::kprintln!("  ns16550 UART + PLIC, virtio-mmio (blk + net), smoltcp + DHCP");
            crate::kprintln!("  preemptive scheduler, U-mode + ecall, ramfs");
        }
        "clear" => {
            crate::kprint!("\u{1b}[2J\u{1b}[H");
        }
        other => {
            crate::kprintln!("unknown command: '{}' (try 'help')", other);
        }
    }
}

/// Join the remaining whitespace-split tokens back into a single string.
fn collect_rest<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    let mut s = String::new();
    for (i, p) in parts.enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(p);
    }
    s
}
