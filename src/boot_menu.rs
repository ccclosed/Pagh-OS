//! The boot-time disk menu: the only place a partition may be formatted.
//!
//! WHY A MENU AND NOT A POLICY. The kernel cannot tell a partition the operator
//! wants for pagh from one that merely *looks* empty, and it certainly cannot
//! tell whether the ext2 superblock it sees belongs to pagh or to somebody's
//! Linux root. Guessing wrong costs data. So the boot path mounts only a volume
//! that already carries pagh's journal marker, and everything else — formatting a
//! partition, or taking over one that holds another filesystem — is a decision a
//! human makes here, with the list in front of them.
//!
//! SAFETY RULES THIS ENFORCES
//! * `Enter` never formats and never creates a filesystem: on a mountable volume
//!   it mounts, and on anything else it explains what `F` would do. Formatting
//!   requires `F` and then typing the device name, so a stray Enter cannot do it;
//! * a partition holding a foreign filesystem is shown as such and formatting it
//!   needs the same typed confirmation, with the consequence spelled out;
//! * if nobody touches the keyboard, the menu times out into the non-destructive
//!   choice (mount a pagh volume if there is one, otherwise use the RAM disk);
//! * an empty list is never shown — with nothing to choose the boot continues
//!   silently, so an unattended machine is unaffected.
//!
//! Drawn straight into the framebuffer console rather than through the shell:
//! this runs before the shell exists, and it has to work when the filesystem —
//! the thing being chosen — does not.
//!
//! INPUT. USB is polled first (`xhci::poll`), PS/2 second: the menu runs inside
//! `init_fs`, where interrupts are off, so neither the USB thread nor the PS/2 IRQ
//! handler can be relied on. Both drivers are polled synchronously here instead,
//! which is also how the menu works on the machine it was written for (no PS/2
//! controller at all). When neither produces a key the menu simply times out.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::framebuffer;
use crate::drivers::partition::PartitionContent;
use crate::drivers::BlockDevice;
use crate::shell::keys::{Decoder, KeyEvent};

/// What the operator decided.
pub enum Choice {
    /// Continue with whatever the boot path would have done on its own.
    Skip,
    /// Mount this device at `/mnt`. Never formats, even if the mount fails.
    Mount(Arc<dyn BlockDevice>),
    /// Format this device (ext2 + WAL) and mount it. Only ever returned after a
    /// typed confirmation.
    Format(Arc<dyn BlockDevice>),
}

/// One row of the menu.
struct Row {
    dev: Arc<dyn BlockDevice>,
    label: String,
    size: String,
    content: String,
    /// Formatting this row destroys a filesystem pagh did not create.
    foreign: bool,
    /// Mounting this row would have pagh write inside somebody else's
    /// filesystem (the driver rewrites group descriptors on mount).
    mount_warns: bool,
    /// There is no filesystem here to mount: `F` is the only way in.
    needs_format: bool,
}

/// Seconds the menu waits before taking the safe default.
const TIMEOUT_SECS: u64 = 20;

/// Show the menu when there is something to choose.
///
/// `partitions` is the boot path's [`crate::drivers::partition::candidates`] list:
/// the caller classifies devices once and both the menu and the fallback path use
/// that same answer, so the list the operator sees is the list the boot path acts
/// on. The RAM disk is added here.
///
/// Returns [`Choice::Skip`] immediately when the machine exposes nothing to choose
/// (a plain RAM-disk boot has nothing to ask about) or when the framebuffer is
/// unavailable, so an unattended boot is never blocked.
pub fn run(partitions: &[(Arc<dyn BlockDevice>, PartitionContent)]) -> Choice {
    let rows = collect(partitions);
    if rows.is_empty() || !framebuffer::is_initialized() {
        return Choice::Skip;
    }

    // Start on the first row that is not somebody else's filesystem. Candidates
    // are ordered pagh-volume-first, so this lands on pagh's own volume when there
    // is one and on a blank partition otherwise — Enter is safe on both.
    let mut selected = rows.iter().position(|r| !r.foreign).unwrap_or(0);
    let mut decoder = Decoder::new();
    let mut typed: Option<String> = None;
    let mut message: Option<String> = None;
    let mut remaining = TIMEOUT_SECS;

    loop {
        draw(
            &rows,
            selected,
            typed.as_deref(),
            message.as_deref(),
            remaining,
        );

        // One second at a time: the countdown is redrawn between slices, and a
        // keypress ends the wait immediately.
        let events = wait_ms_polling(1000, &mut decoder);
        // The countdown freezes while a device name is being typed: a format
        // prompt that expires mid-typing would cancel itself silently, at the
        // worst possible moment.
        if typed.is_none() {
            remaining = remaining.saturating_sub(1);
        }

        for event in events {
            match (&mut typed, event) {
                // Confirmation prompt: collect the device name.
                (Some(buffer), KeyEvent::Char(c)) => {
                    if buffer.len() < 32 {
                        buffer.push(c);
                    }
                }
                (Some(buffer), KeyEvent::Backspace) => {
                    buffer.pop();
                }
                (Some(_), KeyEvent::Escape) => {
                    typed = None;
                    message = Some(String::from("format cancelled"));
                }
                (Some(buffer), KeyEvent::Enter) => {
                    let wanted = rows[selected].dev.name();
                    if buffer.trim() == wanted {
                        return Choice::Format(Arc::clone(&rows[selected].dev));
                    }
                    message = Some(format!("name does not match - expected {}", wanted));
                    typed = None;
                }
                (None, KeyEvent::Up) => selected = selected.saturating_sub(1),
                (None, KeyEvent::Down) => {
                    if selected + 1 < rows.len() {
                        selected += 1;
                    }
                }
                (None, KeyEvent::Enter) => {
                    let row = &rows[selected];
                    if row.mount_warns {
                        // Mounting would write inside somebody else's filesystem.
                        // Refuse the key and point at the only safe route in.
                        message = Some(String::from(
                            "FOREIGN fs - Enter will not mount it; F formats it (destroys it) or Esc",
                        ));
                    } else if row.needs_format {
                        // Nothing to mount here. Enter must not silently create a
                        // filesystem: only F does that, and only after the typed
                        // confirmation below.
                        message = Some(String::from(
                            "nothing to mount here - F formats it (typing its name is required)",
                        ));
                    } else {
                        return Choice::Mount(Arc::clone(&row.dev));
                    }
                }
                (None, KeyEvent::Char(c)) => match c {
                    'f' | 'F' => {
                        typed = Some(String::new());
                        message = Some(if rows[selected].foreign {
                            String::from("WARNING: FOREIGN fs - formatting DESTROYS its contents")
                        } else {
                            String::from("formatting: type the device name and press Enter")
                        });
                    }
                    's' | 'S' => return Choice::Skip,
                    _ => {}
                },
                (None, KeyEvent::Escape) => return Choice::Skip,
                _ => {}
            }
        }

        if remaining == 0 {
            // The safe default: the boot path's own choice, which mounts a pagh
            // volume when one exists and never formats a partition. Returning a
            // synthetic Mount here would log "the operator chose to mount" for an
            // operator who was not there.
            return Choice::Skip;
        }
    }
}

/// Read one key, driving every input device the kernel has.
///
/// USB FIRST, and by polling. The menu runs inside `init_fs`, where interrupts
/// are still disabled, so:
/// * the PS/2 IRQ handler is not running — but that is the lesser problem, since
///   the machine this menu exists for has no PS/2 controller at all;
/// * the `usb` kernel thread is not scheduled either — but it does not need to
///   be: `xhci::poll()` drives the controller synchronously, and it is polled by
///   design (no interrupt endpoint is used anywhere in that driver). Calling it
///   here gives the boot menu a live USB keyboard.
/// The HID reports it collects are translated into the same PS/2 set-1 scancode
/// stream every other consumer reads, so the menu decodes one format regardless
/// of which keyboard produced it.
///
/// PS/2 is still polled afterwards, for free: machines that do have it work
/// without any USB at all.
fn poll_key(usb: bool) -> Option<u8> {
    if usb {
        crate::drivers::xhci::poll();
    }
    if let Some(byte) = crate::drivers::ps2_kbd::take_scancode() {
        return Some(byte);
    }
    use x86_64::instructions::port::Port;
    // SAFETY: ports 0x60/0x64 are the PS/2 controller's data and status ports;
    // both are reads, and the status byte is checked before the data byte.
    unsafe {
        let mut status: Port<u8> = Port::new(0x64);
        if status.read() & 1 == 0 {
            return None;
        }
        let mut data: Port<u8> = Port::new(0x60);
        Some(data.read())
    }
}

/// Busy-wait for `ms` milliseconds using the PIT, polling the keyboard as it
/// goes. Returns the first scancode seen.
///
/// WHY NOT `ticks()`: it only advances while interrupts are enabled (an
/// invariant of this kernel), and the menu runs with them off — a deadline
/// measured in ticks would never arrive. The PIT runs from its own crystal and
/// is unaffected by IF, so a one-shot on channel 2 is the clock here. `hlt` is
/// equally unusable: with IF=0 nothing would ever wake the CPU.
fn wait_ms_polling(ms: u32, decoder: &mut Decoder) -> Vec<KeyEvent> {
    use x86_64::instructions::port::Port;

    const PIT_FREQ: u64 = 1_193_182;
    let mut events = Vec::new();
    for _ in 0..ms {
        // One-millisecond one-shot on channel 2 (mode 0, terminal count).
        let count = (PIT_FREQ / 1000) as u16 - 1;
        // SAFETY: standard PIT ports; the speaker is left off and channel 2 is
        // gated on so the counter runs.
        unsafe {
            let mut cmd: Port<u8> = Port::new(0x43);
            let mut ch2: Port<u8> = Port::new(0x42);
            let mut gate: Port<u8> = Port::new(0x61);
            let saved = gate.read();
            gate.write((saved & !0x02) | 0x01);
            cmd.write(0b1011_0000);
            ch2.write((count & 0xFF) as u8);
            ch2.write((count >> 8) as u8);

            let mut spins = 0u32;
            loop {
                // The xHCI poll costs MMIO reads and a report-ring walk, so it is
                // throttled: the PS/2 status port is polled every spin, USB every
                // 256 spins (a fraction of a millisecond). Polling the controller
                // at the spin rate made the countdown far slower than real time.
                let usb = spins % 256 == 0;
                if let Some(byte) = poll_key(usb) {
                    if let Some(event) = decoder.feed(byte) {
                        events.push(event);
                    }
                }
                if gate.read() & 0x20 != 0 {
                    break;
                }
                spins += 1;
                if spins > 2_000_000 {
                    break; // No PIT: fall through rather than hang the boot.
                }
                core::hint::spin_loop();
            }
            gate.write(saved);
        }
    }
    events
}

/// Every choice the menu offers: the caller's partitions, plus the RAM disk when
/// the boot medium carries one.
///
/// The RAM disk is classified like any other device rather than assumed empty: a
/// boot medium may carry a formatted pagh volume (`disk.img` next to the kernel),
/// and then mounting it is the right answer, not formatting it.
fn collect(partitions: &[(Arc<dyn BlockDevice>, PartitionContent)]) -> Vec<Row> {
    let mut rows: Vec<Row> = partitions
        .iter()
        .map(|(dev, content)| row_for(Arc::clone(dev), *content, false))
        .collect();
    if let Some(ram) = crate::drivers::get_block("ram0") {
        let content = crate::drivers::partition::classify(&*ram);
        rows.push(row_for(ram, content, true));
    }
    rows
}

/// Turn one classified device into a menu row.
///
/// `in_ram` only changes the label: the RAM disk is worth calling out, because a
/// choice that looks persistent can be one that evaporates at power-off.
fn row_for(dev: Arc<dyn BlockDevice>, content: PartitionContent, in_ram: bool) -> Row {
    let sectors = dev.sector_count();
    let (mut text, foreign, mount_warns, needs_format): (String, bool, bool, bool) = match content {
        PartitionContent::PaghVolume => {
            (String::from("pagh volume (ext2+wal)"), false, false, false)
        }
        PartitionContent::ForeignExt2 => (String::from("FOREIGN fs (ext2/ext4)"), true, true, true),
        PartitionContent::Blank => (String::from("empty"), false, false, true),
        PartitionContent::Other => (String::from("unknown data"), true, true, true),
    };
    if in_ram {
        text.push_str(" [RAM]");
    }
    Row {
        label: String::from(dev.name()),
        dev,
        size: format!("{} MiB", sectors * 512 / (1024 * 1024)),
        content: text,
        foreign,
        mount_warns,
        needs_format,
    }
}

/// Paint the whole menu.
///
/// Drawn with the framebuffer console's text interface rather than cell-by-cell
/// painting: `FbWriter` exposes the text cursor, a foreground colour and
/// `clear`, and a menu that only needs lines of text should not reach past that
/// into the writer's pixel API. The current row is drawn in the theme's accent
/// colour and prefixed with `>`.
fn draw(rows: &[Row], selected: usize, typed: Option<&str>, message: Option<&str>, remaining: u64) {
    let console = framebuffer::console();
    let (fg, _bg) = framebuffer::colors();
    // The accent is the current theme's prompt colour: the menu is drawn before
    // the shell exists, but the palette is already decided, so the highlighted
    // row matches what the console will look like a second later.
    let settings = crate::shell::theme::snapshot();
    let accent =
        crate::shell::theme::THEMES[settings.theme % crate::shell::theme::THEMES.len()].prompt;
    let width = console.text_grid().0.min(78);
    let inner = width.saturating_sub(2);

    // EVERY string here is plain ASCII on purpose: the kernel font covers
    // 0x20..=0x7E only, so a Cyrillic label renders as nothing at all (and the
    // first version of this menu came up with half its text missing because of
    // exactly that). Translating the UI is a font-side job, not a call-site one.
    console.clear();
    console.set_fg_color(fg);
    crate::fb_println!("+{}+", "-".repeat(inner));
    crate::fb_println!(
        "|{}|",
        center(
            &format!("pagh {} - disk menu", env!("CARGO_PKG_VERSION")),
            inner
        )
    );
    crate::fb_println!("+{}+", "-".repeat(inner));
    crate::fb_println!(
        "|{:<width$}|",
        "  #  device              size       contents",
        width = inner
    );

    for (index, row) in rows.iter().enumerate() {
        // One string per row, printed once: switching the colour between fields
        // mid-line left the tail of the row unpainted.
        let text = format!(
            "{}{:>2}  {:<18}{:>9}  {}",
            if index == selected { ">" } else { " " },
            index + 1,
            truncate(&row.label, 18),
            row.size,
            truncate(&row.content, inner.saturating_sub(36)),
        );
        console.set_fg_color(if index == selected { accent } else { fg });
        crate::fb_println!("|{:<width$}|", text, width = inner);
    }
    console.set_fg_color(fg);
    crate::fb_println!("+{}+", "-".repeat(inner));

    match typed {
        Some(buffer) => {
            // The message is the warning here ("formatting DESTROYS ..."), so it
            // has to stay on screen while the name is being typed — dropping it
            // was a real defect: the operator typed the confirmation without the
            // sentence that explains what it does.
            if let Some(text) = message {
                crate::fb_println!("  {}", text);
            } else {
                crate::fb_println!();
            }
            crate::fb_println!(
                "  CONFIRM: type the device name exactly, then Enter - {}",
                rows[selected].dev.name()
            );
            console.set_fg_color(accent);
            crate::fb_println!("  > {}", buffer);
            console.set_fg_color(fg);
        }
        None => {
            crate::fb_println!(
                "  Up/Down select  Enter mount  F format  Esc skip  (nothing is auto-formatted)"
            );
            if let Some(text) = message {
                crate::fb_println!("  {}", text);
            } else {
                crate::fb_println!();
            }
        }
    }

    // The countdown line exists only when the menu is idle. While the confirmation
    // prompt is open the countdown is frozen (see `run`), so printing "no key in Ns"
    // there would promise something the code does not do.
    if typed.is_none() {
        let footer = if rows.iter().any(|r| r.content.starts_with("pagh volume")) {
            format!(
                "  no key in {}s: use the pagh volume (no formatting)",
                remaining
            )
        } else {
            format!(
                "  no key in {}s: continue unchanged (no formatting)",
                remaining
            )
        };
        crate::fb_println!("{}", footer);
    }
    // The field report for a machine with no serial port: one line that says
    // whether there is a controller, whether any port has a device, whether the
    // device reached a HID interface and whether reports are arriving. The menu is
    // the only screen a machine with a dead keyboard shows, so it has to answer
    // the question "why is nothing happening" by itself.
    crate::fb_println!();
    crate::fb_println!("  {}", crate::drivers::input_summary());
    {
        let summary = crate::drivers::xhci::summary();
        if summary.probe_mask != 0 {
            let lost = summary.probe_mask & !summary.ccs_mask;
            let mut lost_text = alloc::string::String::new();
            for port in 1..=summary.ports {
                if lost & (1 << (port as u32 - 1)) == 0 {
                    continue;
                }
                if !lost_text.is_empty() {
                    lost_text.push(',');
                }
                lost_text.push_str(&alloc::format!("{}", port));
            }
            crate::fb_println!(
                "  xhci ports: attached before reset={:#010x} now={:#010x} lost=[{}]",
                summary.probe_mask,
                summary.ccs_mask,
                lost_text
            );
        }
    }
    // Controller scan and raw register state: the two things that decide whether
    // "the port does not reset" is our bug or the platform's.
    for (index, info) in crate::drivers::xhci::scan().into_iter().enumerate() {
        crate::kprintln!(
            "xhci {}: {:04x}:{:04x} ports={} attached={:#010x}{}",
            index + 1,
            info.vendor,
            info.device,
            info.ports,
            info.attached,
            if info.chosen { " <- using" } else { "" }
        );
        crate::fb_println!(
            "  xhci {}: {:04x}:{:04x} ports={} attached={:#010x}{}",
            index + 1,
            info.vendor,
            info.device,
            info.ports,
            info.attached,
            if info.chosen { " <- using" } else { "" }
        );
    }
    {
        // Masks, not ranges: see the note in the shell banner. This block used to
        // be printed twice (a stale copy left by an edit), which is why the field
        // photos show the same `xhci proto:` line twice.
        let masks = crate::drivers::xhci::proto_masks();
        crate::fb_println!(
            "  xhci proto: usb2={:#010x} usb3={:#010x}",
            masks[0],
            masks[1]
        );
    }
    for cap in crate::drivers::xhci::xcaps().into_iter().take(6) {
        crate::fb_println!(
            "  xhci xcap {:#06x}: id={} next={:#04x} dw1={:#010x} dw2={:#010x}",
            cap.offset,
            cap.id,
            cap.next,
            cap.dw1,
            cap.dw2
        );
    }
    if let Some(d) = crate::drivers::xhci::raw_dump() {
        crate::fb_println!(
            "  xhci caps: hciver={:#06x} hcsparams1={:#010x} hccparams1={:#010x} legacy={}",
            d.hciversion,
            d.hcsparams1,
            d.hccparams1,
            d.legacy
        );
        crate::fb_println!(
            "  xhci op: caplen={:#x} usbcmd={:#010x} usbsts={:#010x} config={:#x} writes={} halts={} pci_cmd={:#06x}",
            d.caplength,
            d.usbcmd,
            d.usbsts,
            d.config,
            if d.writes_ok { "ok" } else { "IGNORED" },
            d.halts,
            d.pci_cmd
        );
        crate::fb_println!(
            "  xhci port {} regs: {:#010x} {:#010x} {:#010x} {:#010x}",
            d.port,
            d.port_regs[0],
            d.port_regs[1],
            d.port_regs[2],
            d.port_regs[3]
        );
        if d.halts != 0 {
            // The controller halted at least once; these are the register values
            // at the first sighting, which is the only proof of WHY (HSE = DMA,
            // HCE = internal error, RS cleared = software/firmware).
            crate::fb_println!(
                "  xhci halt: count={} usbcmd={:#010x} usbsts={:#010x}",
                d.halts,
                d.halt_cmd,
                d.halt_sts
            );
            crate::kprintln!(
                "xhci halt: count={} usbcmd={:#010x} usbsts={:#010x}",
                d.halts,
                d.halt_cmd,
                d.halt_sts
            );
        }
    }
    // Per-port enumeration state. A few short lines are the difference between
    // "the keyboard does not work" and knowing WHICH step failed, on a machine
    // that cannot run a shell command to ask.
    for row in crate::drivers::xhci::diagnostics().into_iter().take(8) {
        let (ccs, ped, pp, _, pls) = crate::drivers::xhci::decode_portsc(row.portsc);
        crate::fb_println!(
            "  usb {:>2}: proto=usb{} ccs={} ped={} pp={} spd={} pls={} stage={} code={:#x} try={} ms={} pre={:#010x} post={:#010x} prc={} ped_seen={} ped_sc={:#010x}",
            row.port,
            row.proto,
            ccs as u8,
            ped as u8,
            pp as u8,
            row.speed,
            pls,
            row.stage,
            row.code,
            row.attempts,
            row.ms,
            row.pre,
            row.post,
            row.seen_prc as u8,
            row.seen_ped as u8,
            row.ped_sc
        );
    }
    if typed.is_none() && !crate::drivers::any_keyboard() {
        crate::fb_println!(
            "  NO KEYBOARD DETECTED - the menu will continue unchanged when it times out"
        );
    }
    framebuffer::flush();
}

/// Centre `text` in a field of `width` characters.
fn center(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return truncate(text, width);
    }
    let pad = (width - len) / 2;
    format!("{}{}", " ".repeat(pad), text)
}

/// Truncate to `width` characters (byte-safe for the ASCII/UTF-8 mix used here).
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return String::from(text);
    }
    text.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}
