//! `disk` — pick a block device, format it ext2, mount it at `/mnt`.
//!
//! WHY THIS EXISTS. `boot::init_fs` mounts `/mnt` on its own when the machine's
//! disk already carries a filesystem, and formats it only when it is genuinely
//! blank (`fs::format_policy`). Everything else — a partitioned SSD, a disk with
//! a foreign filesystem, a machine with no supported drive at all — ended the
//! boot with `fs (no disk)` and no way forward: there was no `mkfs`, no
//! `fdisk`, and no way to say "yes, use THIS device". This module is that way
//! forward, and it is deliberately interactive: the one operation here is
//! destructive beyond recovery.
//!
//! THE WIZARD OWNS THE NEXT LINE, NOT THE KEYBOARD. `disk setup` prints the
//! device table and arms a pending question; the shell's own REPL reads the
//! answer and hands it here ([`feed_line`]). That reuses the existing line
//! editor, echo, history and caret — a second input loop inside a command would
//! have to re-implement all of it, and would fight the shell for the keyboard.
//!
//! SAFETY MODEL. The classification is `fs::device::probe_boot_area` — the same
//! code the boot path uses, so the menu can never offer to erase a disk the boot
//! path protects. A device that is not blank is formatted only after the operator
//! types its name back (see [`Step::ConfirmName`]); the pure decision that
//! authorises this is `format_policy::format_allowed`'s `allow_destructive`,
//! which exists precisely for "an operator said so". Typing `nvme0` is a
//! statement no boot flag can make by accident.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::fs::format_policy::{format_allowed, Layout, Refusal};
use crate::sync::spinlock::Spinlock;

/// Print one line on the shell's console (the command handlers' own printer).
fn println_text(text: &str) {
    crate::shell::commands::shell_println(text);
}

/// A device as the menu shows it.
struct Entry {
    name: String,
    sectors: u64,
    layout: Layout,
    has_ext2: bool,
}

impl Entry {
    fn size_text(&self) -> String {
        let bytes = self.sectors.saturating_mul(512);
        if bytes >= 1024 * 1024 * 1024 {
            format!("{} GiB", bytes / (1024 * 1024 * 1024))
        } else if bytes >= 1024 * 1024 {
            format!("{} MiB", bytes / (1024 * 1024))
        } else if bytes >= 1024 {
            format!("{} KiB", bytes / 1024)
        } else {
            format!("{} B", bytes)
        }
    }

    /// What the operator is about to do to this device, in one word.
    fn state_text(&self) -> &'static str {
        if self.has_ext2 {
            "ext2 (mountable)"
        } else {
            match self.layout {
                Layout::Blank => "blank (safe to format)",
                Layout::PartitionTable => "partition table (has data)",
                Layout::Ext2 => "ext2 superblock, unusable (has data)",
                Layout::Foreign => "unrecognised data (has data)",
            }
        }
    }

    /// May this device be formatted without an explicit acknowledgement?
    fn is_blank(&self) -> bool {
        self.layout == Layout::Blank && !self.has_ext2
    }
}

/// Which question the wizard is waiting for an answer to.
enum Step {
    /// `pick <n>` — which device to format.
    ChooseDevice(Vec<Entry>),
    /// `type <name>` — the acknowledgement for a device that holds data.
    ConfirmName { entries: Vec<Entry>, index: usize },
}

static WIZARD: Spinlock<Option<Step>> = Spinlock::new(None);

/// Is a `disk` question waiting for the next line the shell reads?
pub fn pending() -> bool {
    WIZARD.lock().is_some()
}

/// Abandon a pending question (`disk cancel`, or a `^C`-style escape).
pub fn cancel() {
    *WIZARD.lock() = None;
}

/// Enumerate the registered block devices with their on-disk classification.
///
/// Reads the first MiB of every device, so it is not a hot path — it runs when
/// the operator asks for the table, which is exactly when a read error matters
/// and should be visible.
fn entries() -> Vec<Entry> {
    use crate::fs::ext2::Ext2Fs;

    crate::drivers::list_blocks()
        .into_iter()
        .map(|(name, sectors)| {
            let probe = crate::drivers::get_block(&name)
                .map(|blk| crate::fs::device::probe_boot_area(&*blk));
            let has_ext2 = crate::drivers::get_block(&name)
                .map(|blk| Ext2Fs::has_valid_superblock(&*blk))
                .unwrap_or(false);
            Entry {
                name,
                sectors,
                layout: probe.map(|p| p.layout()).unwrap_or(Layout::Foreign),
                has_ext2,
            }
        })
        .collect()
}

/// `disk` — with no argument, print the table.
pub fn cmd_list() {
    let entries = entries();
    if entries.is_empty() {
        println_text("disk: no block devices");
        println_text(
            "disk: the kernel has drivers for virtio-blk (QEMU) and NVMe. A boot image can also \
             carry one: see `module_path: boot():/disk.img` in limine.conf.",
        );
        return;
    }
    println_text("  #  DEVICE      SIZE      STATE");
    for (i, e) in entries.iter().enumerate() {
        println_text(&format!(
            "{:>3}  {:<10}  {:<8}  {}",
            i + 1,
            e.name,
            e.size_text(),
            e.state_text()
        ));
    }
    let mounted = crate::vfs::mount_snapshot()
        .iter()
        .any(|m| m.point == "/mnt");
    println_text(&format!(
        "{}",
        if mounted {
            "/mnt is mounted; `disk mount <n>` re-mounts another device (reboot to unmount)"
        } else {
            "nothing is mounted at /mnt; run `disk setup`, or `disk mount <n>` for an ext2 device"
        }
    ));
}

/// `disk setup` — the interactive path: pick, confirm, format, mount.
pub fn cmd_setup() {
    let entries = entries();
    if entries.is_empty() {
        println_text("disk setup: no block devices to choose from");
        return;
    }
    println_text("Format which device as ext2 and mount it at /mnt?");
    for (i, e) in entries.iter().enumerate() {
        println_text(&format!(
            "  [{}] {:<10} {:<8} {}",
            i + 1,
            e.name,
            e.size_text(),
            e.state_text()
        ));
    }
    println_text("Enter a number, or press Enter to cancel.");
    *WIZARD.lock() = Some(Step::ChooseDevice(entries));
}

/// `disk mount <n>` — mount an existing ext2 filesystem; never formats.
pub fn cmd_mount(index: usize) {
    let Some(entry) = entries().into_iter().nth(index) else {
        println_text(&format!("disk mount: no device number {}", index + 1));
        return;
    };
    if !entry.has_ext2 {
        println_text(&format!(
            "disk mount: {} has no usable ext2 filesystem ({})",
            entry.name,
            entry.state_text()
        ));
        return;
    }
    mount(&entry.name);
}

/// `disk format <n> [--force]` — non-interactive format for scripts and E2E.
///
/// A device that is not blank needs `--force`, which is the same acknowledgement
/// the wizard asks for by name; a script that passes it is saying the same thing
/// an operator typing the device name says.
pub fn cmd_format(index: usize, force: bool, mount_after: bool) {
    let Some(entry) = entries().into_iter().nth(index) else {
        println_text(&format!("disk format: no device number {}", index + 1));
        return;
    };
    if !entry.is_blank() && !force {
        println_text(&format!(
            "disk format: {} is not blank ({}); re-run with --force if you mean it",
            entry.name,
            entry.state_text()
        ));
        return;
    }
    format_and_mount_with(&entry.name, force, mount_after);
}

/// Feed one line of shell input to the pending wizard question.
///
/// Called by the REPL INSTEAD of the command dispatcher while [`pending`] is
/// true, so a wizard answer is never mistaken for a command.
pub fn feed_line(line: &str) {
    let line = line.trim();
    let step = WIZARD.lock().take();
    match step {
        None => {}
        Some(Step::ChooseDevice(entries)) => {
            if line.is_empty() {
                println_text("disk setup: cancelled");
                return;
            }
            let Ok(n) = line.parse::<usize>() else {
                println_text(&format!(
                    "disk setup: `{}` is not a number; cancelled",
                    line
                ));
                return;
            };
            let Some(index) = n.checked_sub(1).filter(|i| *i < entries.len()) else {
                println_text(&format!("disk setup: no device number {}", n));
                return;
            };
            let name = entries[index].name.clone();
            let blank = entries[index].is_blank();
            let state = entries[index].state_text();
            if blank {
                format_and_mount(&name, false);
                return;
            }
            // A device with data: the operator must name it back. This is the
            // acknowledgement `format_policy`'s `allow_destructive` is for.
            println_text(&format!(
                "{} holds {} — formatting destroys it.",
                name, state
            ));
            println_text(&format!(
                "Type the device name (`{}`) to confirm, or press Enter to cancel.",
                name
            ));
            *WIZARD.lock() = Some(Step::ConfirmName { entries, index });
        }
        Some(Step::ConfirmName { entries, index }) => {
            let Some(entry) = entries.get(index) else {
                return;
            };
            if line != entry.name {
                println_text(&format!(
                    "disk setup: cancelled (expected `{}`)",
                    entry.name
                ));
                return;
            }
            format_and_mount(&entry.name, true);
        }
    }
}

/// Format `name` and mount the result at `/mnt`.
fn format_and_mount(name: &str, allow_destructive: bool) {
    format_and_mount_with(name, allow_destructive, true)
}

/// Format `name`; mount it too unless `mount_after` is false.
///
/// `--no-mount` exists so a disk can be PREPARED while another filesystem owns
/// `/mnt`: installing a filesystem onto a partition is the step a user wants
/// before switching the mount over, and pagh has no unmount — the alternative
/// would be "you cannot format anything while /mnt is busy".
fn format_and_mount_with(name: &str, allow_destructive: bool, mount_after: bool) {
    let Some(blk) = crate::drivers::get_block(name) else {
        println_text(&format!("disk: {} disappeared", name));
        return;
    };
    if mount_after
        && crate::vfs::mount_snapshot()
            .iter()
            .any(|m| m.point == "/mnt")
    {
        println_text(
            "disk: /mnt is already mounted; use `--no-mount` to just format, or reboot to change the mounted device",
        );
        return;
    }

    // Same classification and same decision as the boot path; a device that is
    // refused here would have been refused there.
    let probe = crate::fs::device::probe_boot_area(&*blk);
    let has_superblock = crate::fs::ext2::Ext2Fs::has_valid_superblock(&*blk);
    if let Err(refusal) = format_allowed(
        probe.layout(),
        has_superblock,
        probe.is_marked() || allow_destructive,
    ) {
        let reason = match refusal {
            Refusal::ExistingFilesystem => "a valid ext2 filesystem is on it",
            other => other.reason(),
        };
        println_text(&format!("disk: refusing to format {}: {}", name, reason));
        return;
    }

    println_text(&format!(
        "disk: formatting {} ({}) as ext2 + WAL...",
        name,
        probe.layout().name()
    ));
    if let Err(e) = crate::fs::ext2::Ext2Fs::format(Arc::clone(&blk) as Arc<dyn BlockDevice>) {
        println_text(&format!("disk: format failed: {:?}", e));
        return;
    }
    if !mount_after {
        println_text(&format!(
            "disk: {} formatted as ext2 + WAL (not mounted; a reboot will use it)",
            name
        ));
        return;
    }
    match crate::fs::ext2::Ext2Fs::mount(Arc::clone(&blk) as Arc<dyn BlockDevice>) {
        Ok(root) => {
            if let Err(e) = crate::vfs::mount_at_named("/mnt", root, "ext2") {
                println_text(&format!("disk: mount at /mnt failed: {:?}", e));
                return;
            }
            println_text(&format!("disk: {} formatted and mounted at /mnt", name));
            // Same follow-up as the boot path: the userland tree the shell and
            // `lxrun` expect only exists once provisioning has seeded /mnt.
            crate::provision::seed();
            println_text("disk: /mnt seeded; try `ls /mnt`");
        }
        Err(e) => println_text(&format!("disk: post-format mount failed: {:?}", e)),
    }
}

/// Mount an already-formatted device at `/mnt`.
fn mount(name: &str) {
    let Some(blk) = crate::drivers::get_block(name) else {
        println_text("disk: no device named that");
        return;
    };
    if crate::vfs::mount_snapshot()
        .iter()
        .any(|m| m.point == "/mnt")
    {
        println_text("disk: /mnt is already mounted; reboot to change the device");
        return;
    }
    match crate::fs::ext2::Ext2Fs::mount(Arc::clone(&blk) as Arc<dyn BlockDevice>) {
        Ok(root) => match crate::vfs::mount_at_named("/mnt", root, "ext2") {
            Ok(()) => {
                println_text(&format!("disk: {} mounted at /mnt", name));
                crate::provision::seed();
            }
            Err(e) => println_text(&format!("disk: mount at /mnt failed: {:?}", e)),
        },
        Err(e) => println_text(&format!("disk: {} cannot be mounted: {:?}", name, e)),
    }
}

/// The shell command: `disk [list|setup|mount N|format N [--force]|cancel]`.
pub fn cmd(args: &[&str]) {
    match args.first().copied() {
        None | Some("list") => cmd_list(),
        Some("setup") | Some("wizard") => cmd_setup(),
        Some("cancel") => {
            cancel();
            println_text("disk: cancelled");
        }
        Some("mount") => match args.get(1).and_then(|a| a.parse::<usize>().ok()) {
            Some(n) if n >= 1 => cmd_mount(n - 1),
            _ => println_text("usage: disk mount <number>"),
        },
        Some("format") => match args.get(1).and_then(|a| a.parse::<usize>().ok()) {
            Some(n) if n >= 1 => cmd_format(
                n - 1,
                args.iter().any(|a| *a == "--force"),
                !args.iter().any(|a| *a == "--no-mount"),
            ),
            _ => println_text("usage: disk format <number> [--force] [--no-mount]"),
        },
        Some(other) => println_text(&format!("disk: unknown subcommand `{}`", other)),
    }
}
