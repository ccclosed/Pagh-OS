// boot.rs — Kernel boot orchestrator
// 64-bit x86_64 Limine kernel in Rust (#![no_std])
//
// This module owns the ordered, fallible kernel initialization sequence that
// previously lived inline in `lib.rs::_start` (Requirement 1). `_start` now
// simply delegates to `boot::start()`.
//
// Structure (Requirement 1):
//  * 1.1 — initialization runs as an explicit ordered sequence of init steps,
//          modeled as one private `fn` per phase invoked in order by `start()`.
//  * 1.3 — each completed step emits a single concise `info!` line; verbose
//          detail stays at `debug!`.
//  * 1.4 — a missing required Limine response or a failed step funnels through
//          the centralized `fatal()` helper, which logs one `error!` line
//          naming the failed step and enters `arch::cpu::halt_loop()`.
//
// NOTE: behavior is intentionally kept identical to the previous inline
// `_start` for now. The inline LAPIC/IOAPIC MMIO mapping stays here (it moves
// to the APIC/memory owners in task 7). `KERNEL_SIZE` is computed precisely
// from the kernel image extent (linker symbols) in `read_limine_responses`.

use core::sync::atomic::Ordering;

use crate::{arch, drivers, memory, net, shell, task, vfs};
use crate::{debug, error, info, warn};
use crate::{BASE_REVISION, HHDM_REQUEST, KERNEL_ADDR_REQUEST, MEMMAP_REQUEST, MODULES_REQUEST};
use crate::{HHDM_OFFSET, KERNEL_BASE, KERNEL_SIZE};

/// Centralized fatal-boot handler (Requirement 1.4).
///
/// Logs exactly one `error!` line naming the failed step, then parks the CPU
/// forever via the safe `arch::cpu` halt primitive. Never returns.
fn fatal(step: &str) -> ! {
    error!("boot: {} failed", step);
    crate::drivers::earlyfb::errorln(format_args!("BOOT FAILED at step: {}", step));
    arch::cpu::halt_loop();
}

/// Announce that a boot step is about to run, on the emergency console.
///
/// Printed BEFORE the step: a step that hangs never prints its `ok`, so the row
/// naming it in progress is exactly the one to look at. (This is how the "stops
/// after heap" report is read: heap finished, so the hang is in the step named
/// `> apic` below it.)
fn step_begin(visible: bool, name: &str) {
    if visible {
        crate::drivers::earlyfb::println(format_args!("  >  {}", name));
    }
}

/// Report a completed boot step on the emergency console (when there is one).
fn step(visible: bool, name: &str) {
    if visible {
        crate::drivers::earlyfb::println(format_args!("  ok  {}", name));
    }
}

/// Run the ordered kernel initialization sequence, then enter the idle loop.
///
/// Each call below is one init step (Requirement 1.1). Steps that depend on a
/// Limine response read it from the request statics in `lib.rs`; missing
/// responses route through [`fatal`] (Requirement 1.4).
pub fn start() -> ! {
    // Step 0: enable SSE before anything else. The kernel and Rust's
    // `extern "x86-interrupt"` handlers emit SSE (`movaps`) instructions, but
    // Limine hands off with SSE not OS-enabled, so this must happen before any
    // FP/SSE use and before interrupts are enabled. See `arch::cpu::enable_sse`.
    arch::cpu::enable_sse();

    // The emergency console goes up before anything that can fail, and it does
    // not need the heap: on a real PC there is no serial port, so without this a
    // failure in any later step leaves a black screen and nothing else. From here
    // on every step prints its name, so a machine that stops shows exactly where.
    // Capture log lines into the file-mirror ring from the very first message:
    // the boot log is what an operator needs after a failure, and it happens
    // before any filesystem exists to write it to (the flusher thread drains the
    // ring once /mnt is mounted).
    crate::logfile::enable_buffering();

    let visible = crate::drivers::earlyfb::init();
    if visible {
        crate::drivers::earlyfb::clear();
        crate::drivers::earlyfb::println(format_args!(
            "pagh {} booting (emergency console)",
            env!("CARGO_PKG_VERSION")
        ));
    }

    step_begin(visible, "init_serial");
    init_serial();
    step(visible, "serial");
    step_begin(visible, "check_base_revision");
    check_base_revision();
    step(visible, "base revision");
    step_begin(visible, "read_limine_responses");
    read_limine_responses();
    step(visible, "limine responses");
    step_begin(visible, "init_descriptor_tables");
    init_descriptor_tables();
    step(visible, "gdt + idt");
    step_begin(visible, "init_syscalls");
    init_syscalls();
    step(visible, "syscalls");
    step_begin(visible, "init_pmm");
    init_pmm();
    step(visible, "pmm");
    step_begin(visible, "init_vmm");
    init_vmm();
    step(visible, "vmm");
    step_begin(visible, "init_heap");
    init_heap();
    step(visible, "heap");
    step_begin(visible, "init_apic");
    init_apic();
    step(visible, "apic");
    step_begin(visible, "init_drivers");
    init_drivers();
    step(visible, "drivers");
    step_begin(visible, "init_virtio");
    init_virtio();
    step(visible, "virtio");
    step_begin(visible, "init_scheduler");
    init_scheduler();
    step(visible, "scheduler");
    step_begin(visible, "init_usb");
    init_usb();
    step(visible, "usb");
    step_begin(visible, "init_vfs");
    init_vfs();
    step(visible, "vfs");
    step_begin(visible, "init_ramdisk");
    init_ramdisk();
    step(visible, "ramdisk");
    step_begin(visible, "init_fs");
    init_fs();
    step(visible, "fs");
    step_begin(visible, "init_net");
    init_net();
    step(visible, "net");

    // Boot is done: erase the emergency trace so the shell starts on a blank
    // screen. The console stays usable — a panic later still prints here.
    if visible {
        crate::drivers::earlyfb::clear();
    }

    // Hand off to the run phase (spawns the shell, enables interrupts, idles).
    kernel_main();
}

/// Step 1: bring up the COM1 serial port (the primary log sink).
fn init_serial() {
    drivers::serial::init();
    info!("serial");
}

/// Step 2: verify Limine handed us a base revision we support.
fn check_base_revision() {
    if !BASE_REVISION.is_supported() {
        fatal("base revision");
    }
    info!("base revision");
}

/// Step 3: read the required Limine responses (HHDM offset + kernel address)
/// and store them into the global address cells in `lib.rs`.
fn read_limine_responses() {
    match HHDM_REQUEST.response() {
        Some(hhdm) => HHDM_OFFSET.store(hhdm.offset, Ordering::Relaxed),
        None => fatal("hhdm"),
    }

    match KERNEL_ADDR_REQUEST.response() {
        Some(kaddr) => {
            KERNEL_BASE.store(kaddr.virtual_base, Ordering::Relaxed);
            // KERNEL_SIZE is the kernel image extent, computed precisely from
            // the linker symbols (`__kernel_end - __kernel_start`) rather than
            // a hardcoded guess. KERNEL_BASE stays the Limine virtual base.
            KERNEL_SIZE.store(memory::layout::kernel_size(), Ordering::Relaxed);
        }
        None => fatal("kernel address"),
    }

    info!("limine responses");
}

/// Step 4: load the GDT (with TSS/IST) and install the IDT.
fn init_descriptor_tables() {
    arch::x86_64::gdt::init();
    arch::x86_64::idt::init();
    info!("gdt + idt");
}

/// Step 4.5: enable the SYSCALL/SYSRET fast system-call interface.
///
/// Must run after `init_descriptor_tables` because `syscall::init` reads the
/// GDT selectors (`Selectors::user_code()` / `kernel_code()`) to program STAR.
fn init_syscalls() {
    arch::x86_64::syscall::init();
    info!("syscalls");
}

/// Step 5: initialize the physical frame allocator from the Limine memmap.
fn init_pmm() {
    match MEMMAP_REQUEST.response() {
        Some(memmap) => memory::pmm::init(memmap),
        None => fatal("memmap"),
    }
    info!("pmm");
}

/// Step 6: initialize the virtual memory manager over the HHDM.
fn init_vmm() {
    let hhdm_offset = HHDM_OFFSET.load(Ordering::Relaxed);
    memory::vmm::init(hhdm_offset);
    info!("vmm");
}

/// Step 7: initialize the kernel heap / global allocator.
fn init_heap() {
    memory::heap::init();
    info!("heap");
}

/// Step 8: initialize the APIC and route the keyboard IRQ.
///
/// The APIC owns its own LAPIC/IOAPIC MMIO mapping (Requirement 7.4):
/// `apic::init()` establishes those mappings internally via `vmm::map_mmio`
/// before touching any registers, so no MMIO mapping happens inline here.
fn init_apic() {
    arch::x86_64::apic::init();

    // Route IRQ1 (keyboard) -> vector 33 and register its handler.
    arch::x86_64::apic::route_irq(1, 33);
    arch::x86_64::apic::register_irq(33, drivers::ps2_kbd::irq_handler);

    // Route IRQ12 (PS/2 mouse) -> vector 44 and register its handler. The
    // mouse device itself is enabled later in `init_drivers` (after the
    // framebuffer is up, so it knows the screen bounds); routing the IOAPIC
    // redirection entry early is harmless because no packets stream until the
    // device's data reporting is turned on.
    arch::x86_64::apic::route_irq(12, 44);
    arch::x86_64::apic::register_irq(44, drivers::ps2_mouse::irq_handler);

    info!("apic");
}

/// Step 9: initialize the device manager (PS/2 keyboard + framebuffer).
///
/// The framebuffer needs no MMIO mapping of its own: Limine maps the
/// framebuffer into the higher half and hands us the already-mapped virtual
/// address (`fb.address()`), so the driver uses that mapping directly.
fn init_drivers() {
    drivers::init();
    info!("drivers");
}

/// Step 9.5: discover and attach virtio devices (PCI enumeration + drivers).
///
/// Runs after `init_drivers` so the heap is up (PCI `enumerate()` allocates a
/// `Vec`). Attaches the virtio-blk disk as a `BlockDevice` ("virtio-blk0"). If
/// no virtio device of a given kind is present, the corresponding attach logs a
/// warning and no-ops so boot is always preserved (R17.4).
///
/// Networking bring-up attaches the e1000 NIC here (`net::init`).
fn init_virtio() {
    let devs = drivers::pci::enumerate();
    drivers::virtio::blk::init_blk(&devs);
    info!("virtio");
}

/// Step 10: initialize the preemptive scheduler.
///
/// Also captures the wall-clock boot time for `/proc/stat`'s `btime` line: the
/// CMOS RTC is readable before interrupts are enabled, and reading it here
/// (rather than lazily in the renderer) makes `btime` the boot instant, as in
/// Linux. A dead/pre-epoch RTC yields `0`, which the file prints verbatim and
/// the contract documents as the honest gap (`docs/procfs.md` §4.8).
fn init_scheduler() {
    task::scheduler::init();
    task::stat::set_boot_unix(arch::x86_64::linux::rtc::now_unix());
    info!("scheduler");
}

/// Step 11: initialize the virtual filesystem.
fn init_vfs() {
    debug!("About to call vfs::init()...");
    vfs::init();
    debug!("vfs::init() returned");
    info!("vfs");
}

/// Step 11.35: bring up a USB host controller, if the machine has one.
///
/// Runs after the PCI walk (which the virtio step already did) and before the
/// filesystem, because a USB keyboard is an INPUT device: the sooner it is
/// alive, the sooner a machine without PS/2 can be typed into. Failure is not
/// fatal — no controller, or one that will not start, leaves the PS/2 path (and
/// whatever the firmware translates into it) as the only input.
fn init_usb() {
    let devices = crate::drivers::pci::enumerate();
    if crate::drivers::xhci::init(&devices) {
        info!("xhci");
    } else {
        debug!("usb: no xHCI controller");
    }
}

/// Step 11.4: register a boot-loaded disk image as a RAM disk.
///
/// WHY: on real hardware the kernel can only mount a device it has a driver for
/// (virtio-blk, NVMe), so a machine with only SATA — or one whose disk the
/// operator does not want to hand over — came up with no `/mnt` at all. Limine
/// can load files next to the kernel, so the system's disk image travels on the
/// boot medium and is mounted from memory. The device is registered under the
/// module's own file name (`disk.img` -> `ram0`), so `disk list` shows where the
/// filesystem came from.
///
/// Selection rule: the FIRST module whose name ends in `.img`. Deliberate and
/// narrow — a module named `pagh.elf` or `notes.txt` is not a disk, and the boot
/// configuration is the only writer of this list.
fn init_ramdisk() {
    let Some(response) = MODULES_REQUEST.response() else {
        return;
    };
    for module in response.modules() {
        let path = module.path();
        if !path.ends_with(".img") {
            continue;
        }
        let data = module.data();
        if data.is_empty() {
            warn!("ramdisk: module {} is empty; skipped", path);
            continue;
        }
        // SAFETY: a Limine module is a kernel-owned buffer in the HHDM that the
        // PMM keeps reserved for the kernel's lifetime, and no other block device
        // aliases it. `drivers::memdisk` only does bounds-checked byte copies.
        let sectors = unsafe {
            crate::drivers::memdisk::register("ram0", data.as_ptr() as usize, data.len())
        };
        if sectors == 0 {
            warn!(
                "ramdisk: module {} is smaller than one sector; skipped",
                path
            );
            continue;
        }
        info!(
            "ramdisk: {} ({} KiB, {} sectors) registered as ram0",
            path,
            data.len() / 1024,
            sectors
        );
        return;
    }
}

/// Step 11.5: bring the ext2 filesystem up on the real virtio-blk/NVMe disk and
/// mount it at `/mnt` (Task 5.1, R5.6/R9.1/R9.2).
///
/// Ordering note: this runs *after* `init_vfs` (not inside `init_virtio`)
/// because `vfs::mount_at` needs the VFS root to already exist. The block
/// device itself was registered earlier in `init_virtio`; heap/pmm/vmm are all
/// up, and interrupts are still disabled during boot init, so the format/mount
/// disk I/O and allocations run on a single, non-preempted path.
///
/// If no virtio-blk or NVMe device is present this logs a warning and continues
/// booting (R17.4). On first boot the device is formatted, but **only when it is
/// genuinely blank or carries the explicit opt-in marker**: the decision lives in
/// [`crate::fs::format_policy`] and every other layout — a valid ext2 superblock,
/// a partition table, or any other non-zero data — is refused by name (issue #33;
/// on real hardware the machine's own SSD is exactly such a device, and the NVMe
/// path takes the whole namespace, not a partition). After a successful mount a
/// one-shot self-demo writes a file under `/mnt` and reads it back, proving the
/// real-disk journaled write+read path end-to-end on a headless boot.
fn init_fs() {
    use crate::fs::ext2::Ext2Fs;
    use crate::fs::format_policy::{format_allowed, Refusal};

    // Attach NVMe before the partition walk: on a real machine that disk is the
    // one with a partition table, and a partition holding an ext2 filesystem has
    // to be visible before the RAM-disk fallback is chosen.
    //
    // This path used to run only when there was no RAM disk, which under BIOS
    // meant it never ran at all — and it turned out to be broken there (see
    // `nvme::bar0_mmio`: a BAR reading back all ones was mapped as if it were a
    // device). It is exercised now, so it is validated now.
    {
        let devices = crate::drivers::pci::enumerate();
        if let Ok(true) = crate::drivers::nvme::attach(&devices) {
            crate::drivers::nvme::register_block_device();
            info!("fs: NVMe controller attached");
        }
    }

    // Partitions, for every whole device registered so far: on a real machine the
    // disk carries a partition table and pagh must be able to use ONE partition
    // without touching the rest (see `drivers::partition`).
    for (name, _sectors) in drivers::list_blocks() {
        if let Some(dev) = drivers::get_block(&name) {
            // Whole devices only: partitions themselves are never re-parsed.
            let is_partition = match name.rfind('p') {
                Some(pos) => {
                    !name[pos + 1..].is_empty()
                        && name[pos + 1..].chars().all(|c| c.is_ascii_digit())
                }
                None => false,
            };
            if !is_partition {
                drivers::partition::register_partitions(&dev);
            }
        }
    }

    // The operator looks at the list before the kernel decides anything. This is
    // the ONLY place a partition may be formatted or a foreign filesystem taken
    // over: the kernel cannot tell a partition meant for pagh from one that
    // merely looks empty, and guessing wrong costs data. The menu times out into
    // the non-destructive default, so an unattended boot is unchanged.
    //
    // Classify once and hand the same list to the menu: what the operator sees
    // must be exactly what the boot path acts on. A skipped menu then reuses it,
    // so no partition is probed twice on the same boot.
    let partitions = drivers::partition::candidates();
    let menu = crate::boot_menu::run(&partitions);

    // SAFETY-CRITICAL, and the reason nothing below formats on its own: absent a
    // menu choice, the boot path only ever MOUNTS a volume that already carries
    // pagh's own journal marker. A structurally valid ext2/ext4 filesystem
    // belonging to somebody else is somebody's Linux system — mounting it rewrites
    // group descriptors and reserves WAL blocks inside it, and formatting it would
    // be worse. Those partitions are listed in the menu and left alone here.
    for (dev, content) in &partitions {
        if matches!(content, drivers::partition::PartitionContent::ForeignExt2) {
            warn!(
                "fs: {} holds a foreign (non-pagh) filesystem; leaving it untouched",
                dev.name()
            );
        }
    }

    // Storage priority: what the operator picked, else the volume pagh itself
    // formatted on the attached disk, else a disk image from the boot medium
    // (the system carried along on the stick), else virtio-blk (QEMU/dev images),
    // else NVMe.
    let mut blk = match &menu {
        crate::boot_menu::Choice::Mount(dev) => {
            info!("fs: operator chose to mount {}", dev.name());
            Some(alloc::sync::Arc::clone(dev))
        }
        crate::boot_menu::Choice::Format(dev) => {
            warn!("fs: operator chose to format {}", dev.name());
            Some(alloc::sync::Arc::clone(dev))
        }
        crate::boot_menu::Choice::Skip => partitions
            .iter()
            .find(|(_, content)| {
                matches!(content, drivers::partition::PartitionContent::PaghVolume)
            })
            .map(|(dev, _)| {
                info!("fs: using partition {} (pagh volume)", dev.name());
                alloc::sync::Arc::clone(dev)
            })
            .or_else(|| {
                drivers::get_block("ram0").inspect(|_| {
                    info!("fs: using the RAM disk from the boot medium");
                })
            })
            .or_else(|| drivers::get_block("virtio-blk0"))
            .or_else(|| {
                let devices = crate::drivers::pci::enumerate();
                if let Ok(true) = crate::drivers::nvme::attach(&devices) {
                    crate::drivers::nvme::register_block_device();
                    info!("fs: using NVMe block device");
                }
                drivers::get_block("nvme0")
            }),
    };
    let blk = match blk.take() {
        Some(b) => b,
        None => {
            warn!("fs: no block device at all; /mnt not mounted");
            warn!("fs: attach a disk, or boot an image that carries one (`module_path: boot():/disk.img`)");
            info!("fs (no disk)");
            return;
        }
    };

    // Format only what a human explicitly asked to format, in the menu, after
    // typing the device name. Everything else here mounts or refuses.
    //
    // This used to key on `fs::format_policy` alone, which meant a blank partition
    // the boot path happened to select was formatted without asking. It is not
    // selected any more unless the operator chose it (menu) or it is the RAM disk
    // (a scratch volume that disappears at power-off) — see below.
    let root = match menu {
        crate::boot_menu::Choice::Format(_) => {
            warn!(
                "fs: formatting {} on the operator's explicit instruction (menu)",
                blk.name()
            );
            if let Err(e) = Ext2Fs::format(blk.clone()) {
                error!("fs: format failed: {:?}; /mnt not mounted", e);
                return;
            }
            match Ext2Fs::mount(blk.clone()) {
                Ok(root) => root,
                Err(e) => {
                    error!("fs: post-format mount failed: {:?}", e);
                    return;
                }
            }
        }
        // An explicit "mount this" is NEVER silently upgraded to "format this":
        // erasing a volume is not a recovery step, and the operator who picked the
        // row did not ask for it. The message says how to get there on purpose.
        crate::boot_menu::Choice::Mount(_) => match Ext2Fs::mount(blk.clone()) {
            Ok(root) => root,
            Err(e) => {
                error!(
                    "fs: mounting {} failed: {:?}; nothing was written - reboot and press F in the disk menu to format it",
                    blk.name(),
                    e
                );
                info!("fs (mount failed, nothing formatted)");
                return;
            }
        },
        crate::boot_menu::Choice::Skip => match Ext2Fs::mount(blk.clone()) {
            Ok(root) => root,
            Err(first_error) => {
                let has_superblock = Ext2Fs::has_valid_superblock(&*blk);
                // The probe runs only here: a failed mount is the sole situation in
                // which erasing the device is even on the table, so the happy path
                // pays nothing for it.
                let probe = crate::fs::device::probe_boot_area(&*blk);
                // The one way to authorise formatting a device that is not blank: an
                // operator deliberately wrote `format_policy::OPT_IN_MARKER` into
                // sector 0. Nothing else — no bootloader flag, no config file — can
                // turn this on, and the marker cannot appear by accident.
                let allow_destructive = probe.is_marked();
                if allow_destructive {
                    warn!(
                        "fs: {} carries the PAGH-FORMAT opt-in marker; formatting over existing data",
                        blk.name()
                    );
                }
                match format_allowed(probe.layout(), has_superblock, allow_destructive) {
                    Ok(()) => {
                        info!(
                            "fs: formatting {} ({}), ext2 + WAL...",
                            blk.name(),
                            probe.layout().name()
                        );
                        if let Err(e) = Ext2Fs::format(blk.clone()) {
                            error!("fs: format failed: {:?}; /mnt not mounted", e);
                            return;
                        }
                        match Ext2Fs::mount(blk.clone()) {
                            Ok(root) => root,
                            Err(e) => {
                                error!("fs: post-format mount failed: {:?}", e);
                                return;
                            }
                        }
                    }
                    Err(refusal) => {
                        match refusal {
                            // Wording kept byte-identical: it is the historical
                            // refusal line and the docs quote it.
                            Refusal::ExistingFilesystem => error!(
                                "fs: existing ext2 mount failed: {:?}; refusing destructive reformat",
                                first_error
                            ),
                            other => error!(
                                "fs: refusing to format {} ({}): {:?}",
                                blk.name(),
                                other.reason(),
                                first_error
                            ),
                        }
                        // No in-guest route forward exists (there is no `mkfs` in the
                        // shell), so name the operator's options instead of leaving
                        // the refusal as the last word on the serial line.
                        error!(
                            "fs: {} left untouched; reboot and use the disk menu (F + the typed \
                             device name), or run `disk setup` at the shell to format it \
                             deliberately, or zero the start of the device (`wipefs -a` / `dd` \
                             from another system) / write the {} marker",
                            blk.name(),
                            crate::fs::format_policy::OPT_IN_MARKER_NAME
                        );
                        return;
                    }
                }
            }
        },
    };

    match vfs::mount_at_named("/mnt", root, "ext2") {
        Ok(()) => {
            info!("ext2 mounted at /mnt");
            fs_boot_demo();
            crate::provision::seed();
        }
        Err(e) => error!("fs: mount_at(/mnt) failed: {:?}", e),
    }
    info!("fs");
}

/// How far into a device [`probe_boot_area`] looks: 1 MiB.
///
/// Enough for a GPT/MBR partition table, an ext2 superblock, and the boot-sector
/// or superblock-at-offset-0 layouts of every filesystem a user is likely to have
/// (NTFS, XFS, LUKS, ZFS, ext4-in-a-partition). btrfs keeps its superblock at
/// 64 KiB — inside the window as well.
///
/// **The window is a real limitation, not a formality**: a layout that keeps
/// everything outside the first MiB — a `mkswap` signature at the tail of the
/// device, mdadm 1.0 superblocks, IMSM/DDF external RAID metadata, or a device
/// whose start was zeroed by an interrupted `wipefs`/`dd` — probes as `Blank`
/// and is formatted. Bounding the probe is what keeps boot from reading a whole
/// SSD; the cost is that "blank" means "blank in the first MiB".
const PROBE_LIMIT: u64 = 1024 * 1024;

/// Step 11.6: bring up networking (Task 6). Enumerates PCI, attaches the
/// e1000 NIC, builds the own TCP/IP stack state, and enables the UDP echo
/// service on port 7. Address acquisition (DHCP, static fallback) and packet
/// servicing run in the net thread spawned from `kernel_main` once interrupts
/// are enabled and the LAPIC tick is advancing the network clock.
///
/// If no virtio-net device is present this logs a warning and continues booting
/// (R17.3). Networking is fully optional — boot is always preserved.
fn init_net() {
    match net::init() {
        Ok(()) => {
            net::udp_echo_enable(7);
            net::tcp_echo_listen(7);
            info!("net");
        }
        Err(e) => {
            warn!("net: no interface available ({:?}); networking disabled", e);
            info!("net (no nic)");
        }
    }
}

/// One-shot boot self-demo (Task 5.5): exercise the real-disk journaled write
/// and read-back path so a headless boot proves it works end-to-end. Writes a
/// known string to `/mnt/bootdemo.txt`, then re-reads it through a fresh VFS
/// lookup and logs PASS/FAIL. Non-fatal: any error is logged and boot proceeds.
fn fs_boot_demo() {
    use crate::vfs::{self, VfsError};

    const CONTENT: &[u8] = b"pagh-ext2-journaled-write-OK";

    let dir = match vfs::lookup_path("/mnt") {
        Ok(d) => d,
        Err(e) => {
            warn!("fs demo: /mnt lookup failed: {:?}", e);
            return;
        }
    };

    // Create or open the demo file (tolerate it already existing from a prior
    // boot, since the disk image persists across runs).
    let file = match dir.create_file("bootdemo.txt") {
        Ok(f) => f,
        Err(VfsError::AlreadyExists) => match dir.lookup("bootdemo.txt") {
            Ok(f) => {
                // The disk persists across boots; a shorter demo string must not
                // leave the previous run's tail in place (write(2) does not
                // truncate, exactly like POSIX).
                let _ = f.truncate(0);
                f
            }
            Err(e) => {
                warn!("fs demo: open existing failed: {:?}", e);
                return;
            }
        },
        Err(e) => {
            warn!("fs demo: create_file failed: {:?}", e);
            return;
        }
    };

    match file.write(0, CONTENT) {
        Ok(n) if n == CONTENT.len() => {}
        Ok(n) => {
            warn!("fs demo: short write ({} of {} bytes)", n, CONTENT.len());
            return;
        }
        Err(e) => {
            warn!("fs demo: write failed: {:?}", e);
            return;
        }
    }
    dir.sync();

    // Read back through a fresh lookup to avoid any cached node state.
    let rfile = match vfs::lookup_path("/mnt/bootdemo.txt") {
        Ok(f) => f,
        Err(e) => {
            warn!("fs demo: read-back lookup failed: {:?}", e);
            return;
        }
    };
    let mut buf = [0u8; 64];
    match rfile.read(0, &mut buf) {
        Ok(n) if n == CONTENT.len() && &buf[..n] == CONTENT => {
            info!(
                "fs demo: /mnt/bootdemo.txt write+read round-trip PASS ({} bytes)",
                n
            );
        }
        Ok(n) => {
            error!("fs demo: read-back MISMATCH ({} bytes): FAIL", n);
        }
        Err(e) => warn!("fs demo: read-back failed: {:?}", e),
    }
}

/// Run phase: spawn the shell thread, enable interrupts, and become the idle
/// loop.
///
/// The old ~5M-cycle busy-spin debug block (used only to observe timer IRQs
/// during bringup) is removed per Requirement 1.5.
fn kernel_main() -> ! {
    // Entropy capability probe (issue #16). `AT_RANDOM` cannot fail closed —
    // glibc reads it unconditionally at process start — so without RDSEED/RDRAND
    // it derives from the mixed boot seed instead of the old predictable
    // xorshift; this line is what tells the operator the fallback is in use
    // BEFORE the first process starts. No-op (no log line) with hardware entropy.
    crate::security::entropy::report_capabilities();

    debug!("Spawning shell thread...");
    task::scheduler::kernel_thread_spawn_named(shell_thread, "shell");
    debug!("Shell thread spawned (PID should be 1)");

    // Spawn the networking poll thread (Task 6, R13.4). It must start once
    // scheduling is running and interrupts are enabled (below), because the
    // The stack clock advances off the LAPIC timer tick and DHCP/echo servicing
    // relies on the periodic poll. If no NIC was attached, `net::poll` is a
    // cheap no-op, so spawning unconditionally is harmless.
    task::scheduler::kernel_thread_spawn_named(net::net_thread, "net");

    // USB poll thread: enumerates newly attached devices and collects HID
    // reports. Spawned unconditionally — with no controller `xhci::poll` is a
    // no-op, and gating it would need a second source of truth about the
    // hardware that the controller itself already provides.
    task::scheduler::kernel_thread_spawn_named(crate::drivers::xhci::usb_thread, "usb");

    // Log mirror: drains the in-memory ring into /mnt/pagh-log.txt. A kernel
    // thread rather than an inline write, because the logger also runs in
    // interrupt and panic contexts, where touching the filesystem would deadlock.
    task::scheduler::kernel_thread_spawn_named(crate::logfile::flush_thread, "logd");
    debug!("Net thread spawned");

    // Spawn the embedded ring-3 test user process (Requirements 13.1 / 13.4).
    //
    // This MUST happen while interrupts are still disabled: `create_user_process`
    // briefly installs the user CR3 to populate the user address space, and a
    // timer tick in that window would let the scheduler observe the foreign CR3.
    // It also programs TSS RSP0 for the ring-3 → ring-0 transition.
    //
    // The process performs a `SYS_WRITE` (observable on serial) then `SYS_EXIT`,
    // dropping itself from the scheduler rotation; the shell thread continues to
    // run and the prompt stays interactive.
    //
    // Skipped when the `lx_selftest` feature is on: that harness launches its own
    // ring-3 `Compat_Process`, and the kernel's TSS `RSP0` is single-task (see
    // `gdt::set_kernel_stack`), so running two ring-3 processes at once would share
    // one kernel stack. Under the feature the compat process is the sole ring-3
    // task, so it can run its `write`/`exit_group` cleanly and be observed.
    #[cfg(not(any(
        feature = "lx_selftest",
        feature = "lx_livetest",
        feature = "lx_bigindex",
        feature = "lx_tlsbig"
    )))]
    match task::process::spawn_test_user_process() {
        Ok(pid) => info!("user test process spawned (pid {})", pid),
        Err(e) => error!("user test process failed: {}", e),
    }

    // Linux-compat boot-time self-test (cargo feature `lx_selftest`). Runs here —
    // after ext2 mount and networking init, while interrupts are still disabled —
    // so it can enqueue a Compat_Process like the native test process above and so
    // the `fetch_no_network` check observes the no-interface-address state before
    // DHCP runs. Compiled out entirely when the feature is off (default).
    #[cfg(feature = "lx_selftest")]
    crate::selftest_lx::run();

    // Post-network checks on a single thread so they run *sequentially* and do
    // not contend for the network pump. The local-mirror `apt` end-to-end test
    // runs FIRST (fast, against the QEMU host gateway), then the external HTTPS
    // smoke test (which may be slow/unreachable depending on the environment and
    // would otherwise monopolize the pump). Compiled out when the feature is off.
    #[cfg(feature = "lx_selftest")]
    task::scheduler::kernel_thread_spawn_named(
        crate::selftest_lx::run_post_net_checks,
        "lx-selftest",
    );

    // Live full-update integration check (cargo feature `lx_livetest`, spec task
    // 11.1). Spawned on its own thread so it runs after interrupts/DHCP are up.
    // Separate from `lx_selftest`: it talks to the live `deb.debian.org` mirror
    // (default HTTPS config, NOT the local mini-repo). Mutually exclusive in
    // practice — if both features are set, the local selftest harness wins and
    // the live check is skipped to avoid two ring-3 processes contending for the
    // single-task kernel stack. Compiled out when the feature is off (default).
    #[cfg(all(feature = "lx_livetest", not(feature = "lx_selftest")))]
    task::scheduler::kernel_thread_spawn_named(
        crate::selftest_lx::run_live_update_check,
        "lx-live",
    );

    // DIAGNOSTIC (Part B): apt-update parse-stage crash repro over the LOCAL big
    // index (cargo feature `lx_bigindex`). Spawned on its own thread so it runs
    // after interrupts/DHCP are up. Points apt at `http://10.0.2.2:8000` (the
    // `tools/mini_repo.py bigindex` mirror) and drives `apt::update()` (or, with
    // `lx_bigindex_inram`, the in-RAM Test-A variant). Compiled out when the
    // feature is off (default).
    #[cfg(feature = "lx_bigindex")]
    task::scheduler::kernel_thread_spawn_named(
        crate::selftest_lx::run_bigindex_check,
        "lx-bigindex",
    );

    // Large-stream HTTPS regression (cargo feature `lx_tlsbig`, issue #19).
    // One multi-MB authenticated `https_get` against the live mirror's full
    // `Packages.gz`, isolating the TLS transport from the index pipeline.
    // Skipped when a broader harness is also enabled, so exactly one live
    // external download runs per boot (and no two ring-3 processes share the
    // single-task kernel stack).
    #[cfg(all(
        feature = "lx_tlsbig",
        not(any(feature = "lx_selftest", feature = "lx_livetest"))
    ))]
    task::scheduler::kernel_thread_spawn_named(crate::selftest_lx::run_tls_big_check, "lx-tlsbig");

    // The first-boot base userland (glibc + python3)
    // download is opt-in now. shell_thread asks Y/n on the console first and
    // spawns crate::provision::ensure_base_packages_thread only on consent.

    arch::cpu::enable_interrupts();
    info!("interrupts enabled");

    // Main thread becomes the idle loop.
    arch::cpu::halt_loop()
}

/// The shell kernel thread: let the scheduler stabilize, then run the shell.
fn shell_thread() {
    // Give scheduler time to stabilize.
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }

    // The first boot no longer stops to ask about downloading python3: a fresh
    // disk gets the file skeleton from `provision::seed()`, and the glibc/python
    // userland is installed on demand from the shell (`apt update` +
    // `apt install python3`). A boot-time question blocked the shell on a console
    // prompt with no default timeout, which is not something boot may do.
    shell::shell_main();
}
